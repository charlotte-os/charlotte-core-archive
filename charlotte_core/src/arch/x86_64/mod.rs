//! # x86_64 Architecture Module
//! This module implements the Arch interface for the x86_64 instruction set architecture (ISA).

use core::fmt::Write;
use core::str;
use core::{
    borrow::{Borrow, BorrowMut},
    ptr::addr_of,
};

use spin::lazy::Lazy;
use spin::mutex::spin::SpinMutex;

use cpu::*;
use gdt::{tss::Tss, Gdt};
use idt::*;
use serial::{ComPort, SerialPort};

use crate::acpi::{parse, AcpiInfo};
use crate::arch::x86_64::interrupts::apic::Apic;
use crate::arch::x86_64::interrupts::isa_handler::register_iv_handler;
use crate::arch::{HwTimerMode, IsaParams, PagingParams};
use crate::framebuffer::colors::Color;
use crate::framebuffer::framebuffer::FRAMEBUFFER;
use crate::logln;
use crate::memory::pmm::PHYSICAL_FRAME_ALLOCATOR;

mod cpu;
mod exceptions;
mod gdt;
mod global;
mod idt;
mod interrupts;
mod serial;

/// The Api struct is used to provide an implementation of the ArchApi trait for the x86_64 architecture.
pub struct Api {
    acpi_info: AcpiInfo,
    bsp_apic: Apic,
    irq_flags: u64,
}

static BSP_RING0_INT_STACK: [u8; 4096] = [0u8; 4096];
static BSP_TSS: Lazy<Tss> = Lazy::new(|| Tss::new(addr_of!(BSP_RING0_INT_STACK) as u64));
static BSP_GDT: Lazy<Gdt> = Lazy::new(|| Gdt::new(&BSP_TSS));
static BSP_IDT: SpinMutex<Idt> = SpinMutex::new(Idt::new());

pub const X86_ISA_PARAMS: IsaParams = IsaParams {
    paging: PagingParams {
        page_size: 0x1000,
        page_shift: 0xC,
        page_mask: !0xfff,
    },
};

/// Provide the implementation of the Api trait for the Api struct
impl crate::arch::Api for Api {
    type Api = Api;
    /// Define the logger type
    type DebugLogger = SerialPort;
    type Serial = SerialPort;

    fn isa_init() -> Self {
        FRAMEBUFFER.lock().clear_screen(Color::BLACK);

        logln!("Initializing the bootstrap processor");
        Api::init_bsp();
        logln!("============================================================\n");
        logln!("Parsing ACPI information");
        let tbls = parse();
        logln!("============================================================\n");
        let mut api = Api {
            acpi_info: tbls,
            bsp_apic: Apic::new(tbls.madt()),
            irq_flags: 0,
        };
        logln!("============================================================\n");

        logln!("Enable interrupts");
        api.init_interrupts();
        logln!("Bus frequency is: {}MHz", api.bsp_apic.tps / 10000000);
        logln!("============================================================\n");

        logln!("Memory self test");
        Self::pmm_self_test();
        logln!("============================================================\n");

        logln!("All x86_64 sanity checks passed, kernel main has control now");
        logln!("============================================================\n");

        api
    }

    /// Get a new logger instance
    fn get_logger() -> Self::DebugLogger {
        SerialPort::try_new(ComPort::COM1).unwrap()
    }

    fn get_serial(&self) -> Self::Serial {
        SerialPort::try_new(ComPort::COM1).unwrap()
    }

    /// Get the number of significant physical address bits supported by the current CPU
    fn get_paddr_width() -> u8 {
        *PADDR_SIG_BITS
    }
    /// Get the number of significant virtual address bits supported by the current CPU
    fn get_vaddr_width() -> u8 {
        *VADDR_SIG_BITS
    }

    /// Halt the calling LP
    fn halt() -> ! {
        unsafe { asm_halt() }
    }

    /// Kernel Panic
    fn panic() -> ! {
        unsafe { asm_halt() }
    }

    /// Read a byte from the specified port
    fn inb(port: u16) -> u8 {
        asm_inb(port)
    }

    /// Write a byte to the specified port
    fn outb(port: u16, val: u8) {
        asm_outb(port, val)
    }

    /// Initialize the bootstrap processor (BSP)
    ///
    ///  Initialize the application processors (APs)
    fn init_ap(&mut self) {
        //! This routine is run by each application processor to initialize itself prior to being handed off to the scheduler.
    }

    fn setup_isa_timer(&mut self, tps: u32, mode: HwTimerMode, _: u16) {
        let mut divisor = 1u8;
        let mut counter = 0u64;
        while divisor < 128 {
            counter = (self.bsp_apic.tps / divisor as u64) / (tps as u64 * 10);
            if counter < u32::MAX as u64 {
                break;
            }
            divisor <<= 1;
        }
        logln!(
            "Setting up ISA timer with divisor: {}, counter: {}",
            divisor,
            counter
        );
        self.bsp_apic
            .setup_timer(mode.into(), counter as u32, divisor.into());
    }

    fn start_isa_timers(&self) {
        self.bsp_apic.start_timer()
    }

    fn pause_isa_timers(&self) {
        todo!()
    }

    fn interrupts_enabled(&self) -> bool {
        asm_are_interrupts_enabled()
    }

    fn disable_interrupts(&mut self) {
        irq_disable();
    }

    fn restore_interrupts(&mut self) {
        irq_restore();
    }

    fn init_interrupts(&mut self) {
        self.bsp_apic.enable(BSP_IDT.lock().borrow_mut());
    }

    fn set_interrupt_handler(&mut self, h: fn(vector: u64), vector: u32) {
        if vector > u8::MAX as u32 {
            panic!("X86_64 can only have from iv 32 to iv 255 set");
        }
        register_iv_handler(h, vector as u8);
    }

    #[inline(always)]
    fn end_of_interrupt() {
        Apic::signal_eoi();
    }
}

impl Api {
    /// Get the number of significant physical address bits supported by the current CPU
    fn get_paddr_width() -> u8 {
        *PADDR_SIG_BITS
    }
    /// Get the number of significant virtual address bits supported by the current CPU
    fn get_vaddr_width() -> u8 {
        *VADDR_SIG_BITS
    }

    fn init_bsp() {
        //! This routine is run by the bootstrap processor to initialize itself prior to bringing up the kernel.
        logln!("Processor information:");
        BSP_GDT.load();
        logln!("Loaded GDT");
        Gdt::reload_segment_regs();
        logln!("Reloaded segment registers");
        Gdt::load_tss();
        logln!("Loaded TSS");

        logln!("Registering exception ISRs in the IDT");
        exceptions::load_exceptions(BSP_IDT.lock().borrow_mut());
        logln!("Exception ISRs registered");

        logln!("Attempting to load IDT");
        BSP_IDT.lock().borrow().load();
        logln!("Loaded IDT");

        let mut vendor_string = [0u8; 12];
        unsafe { asm_get_vendor_string(&mut vendor_string) }
        logln!("CPU Vendor ID: {}", str::from_utf8(&vendor_string).unwrap());
    }

    fn pmm_self_test() {
        logln!(
            "Number of Significant Physical Address Bits Supported: {}",
            Api::get_paddr_width()
        );
        logln!(
            "Number of Significant Virtual Address Bits Supported: {}",
            Api::get_vaddr_width()
        );

        logln!("Testing Physical Memory Manager");
        logln!("Performing single frame allocation and deallocation test.");
        let alloc = PHYSICAL_FRAME_ALLOCATOR.lock().allocate();
        let alloc2 = PHYSICAL_FRAME_ALLOCATOR.lock().allocate();
        match alloc {
            Ok(frame) => {
                logln!("Allocated frame with physical base address: {:?}", frame);
                let _ = PHYSICAL_FRAME_ALLOCATOR.lock().deallocate(frame);
                logln!("Deallocated frame with physical base address: {:?}", frame);
            }
            Err(e) => {
                logln!("Failed to allocate frame: {:?}", e);
            }
        }
        let alloc3 = PHYSICAL_FRAME_ALLOCATOR.lock().allocate();
        logln!("alloc2: {:?}, alloc3: {:?}", alloc2, alloc3);
        let _ = PHYSICAL_FRAME_ALLOCATOR.lock().deallocate(alloc2.unwrap());
        let _ = PHYSICAL_FRAME_ALLOCATOR.lock().deallocate(alloc3.unwrap());
        logln!("Single frame allocation and deallocation test complete.");
        logln!("Performing contiguous frame allocation and deallocation test.");
        let contiguous_alloc = PHYSICAL_FRAME_ALLOCATOR.lock().allocate_contiguous(256, 64);
        match contiguous_alloc {
            Ok(frame) => {
                logln!(
                    "Allocated physically contiguous region with physical base address: {:?}",
                    frame
                );
                let _ = PHYSICAL_FRAME_ALLOCATOR.lock().deallocate(frame);
                logln!(
                    "Deallocated physically contiguous region with physical base address: {:?}",
                    frame
                );
            }
            Err(e) => {
                logln!("Failed to allocate contiguous frames: {:?}", e);
            }
        }
        logln!("Contiguous frame allocation and deallocation test complete.");
        logln!("Physical Memory Manager test suite finished.");
    }
}
