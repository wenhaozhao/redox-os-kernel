/// This function is where the kernel sets up IRQ handlers
/// It is increcibly unsafe, and should be minimal in nature
/// It must create the IDT with the correct entries, those entries are
/// defined in other files inside of the `arch` module

use core::slice;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::memory::{Frame};
use crate::paging::{Page, PAGE_SIZE, PhysicalAddress, VirtualAddress};

use crate::allocator;
use crate::device;
#[cfg(feature = "graphical_debug")]
use crate::devices::graphical_debug;
use crate::init::device_tree;
use crate::interrupt;
use crate::log::{self, info};
use crate::paging::{self, KernelMapper};

/// Test of zero values in BSS.
static BSS_TEST_ZERO: usize = 0;
/// Test of non-zero values in data.
static DATA_TEST_NONZERO: usize = 0xFFFF_FFFF_FFFF_FFFF;
/// Test of zero values in thread BSS
#[thread_local]
static mut TBSS_TEST_ZERO: usize = 0;
/// Test of non-zero values in thread data.
#[thread_local]
static mut TDATA_TEST_NONZERO: usize = 0xFFFF_FFFF_FFFF_FFFF;

pub static KERNEL_BASE: AtomicUsize = AtomicUsize::new(0);
pub static KERNEL_SIZE: AtomicUsize = AtomicUsize::new(0);
pub static CPU_COUNT: AtomicUsize = AtomicUsize::new(0);
pub static AP_READY: AtomicBool = AtomicBool::new(false);
static BSP_READY: AtomicBool = AtomicBool::new(false);

#[repr(packed)]
pub struct KernelArgs {
    kernel_base: usize,
    kernel_size: usize,
    stack_base: usize,
    stack_size: usize,
    env_base: usize,
    env_size: usize,
    dtb_base: usize,
    dtb_size: usize,
    areas_base: usize,
    areas_size: usize,

    /// The physical base 64-bit pointer to the contiguous bootstrap/initfs.
    bootstrap_base: usize,
    /// Size of contiguous bootstrap/initfs physical region, not necessarily page aligned.
    bootstrap_size: usize,
    /// Entry point the kernel will jump to.
    bootstrap_entry: usize,
}

/// The entry to Rust, all things must be initialized
#[no_mangle]
pub unsafe extern "C" fn kstart(args_ptr: *const KernelArgs) -> ! {
    let bootstrap = {
        let args = &*args_ptr;

        // BSS should already be zero
        {
            assert_eq!(BSS_TEST_ZERO, 0);
            assert_eq!(DATA_TEST_NONZERO, 0xFFFF_FFFF_FFFF_FFFF);
        }

        KERNEL_BASE.store(args.kernel_base, Ordering::SeqCst);
        KERNEL_SIZE.store(args.kernel_size, Ordering::SeqCst);

        //TODO: Remove hard-coded UART for QEMU virt machine
        {
            let mut serial_port = crate::device::uart_pl011::SerialPort::new(
                crate::PHYS_OFFSET + 0x9000000
            );
            serial_port.init(false);
            serial_port.write(b"UART\n");
            *crate::device::serial::COM1.lock() = Some(serial_port);
        }

        // Try to find serial port prior to logging
        device::serial::init_early(crate::PHYS_OFFSET + args.dtb_base, args.dtb_size);

        // Convert env to slice
        let env = slice::from_raw_parts((args.env_base + crate::PHYS_OFFSET) as *const u8, args.env_size);

        // Set up graphical debug
        #[cfg(feature = "graphical_debug")]
        graphical_debug::init(env);

        // Initialize logger
        log::init_logger(|r| {
            use core::fmt::Write;
            let _ = write!(
                crate::debug::Writer::new(),
                "{}:{} -- {}\n",
                r.target(),
                r.level(),
                r.args()
            );
        });

        info!("Redox OS starting...");
        info!("Kernel: {:X}:{:X}", {args.kernel_base}, args.kernel_base + args.kernel_size);
        info!("Stack: {:X}:{:X}", {args.stack_base}, args.stack_base + args.stack_size);
        info!("Env: {:X}:{:X}", {args.env_base}, args.env_base + args.env_size);
        info!("RSDPs: {:X}:{:X}", {args.dtb_base}, args.dtb_base + args.dtb_size);
        info!("Areas: {:X}:{:X}", {args.areas_base}, args.areas_base + args.areas_size);
        info!("Bootstrap: {:X}:{:X}", {args.bootstrap_base}, args.bootstrap_base + args.bootstrap_size);
        info!("Bootstrap entry point: {:X}", {args.bootstrap_entry});

        // Setup interrupt handlers
        core::arch::asm!(
            "
            ldr {tmp}, =exception_vector_base
            msr vbar_el1, {tmp}
            ",
            tmp = out(reg) _,
        );

        /* NOT USED WITH UEFI
        device_tree::fill_memory_map(crate::PHYS_OFFSET + dtb_base, dtb_size);

        let env_size = device_tree::fill_env_data(crate::PHYS_OFFSET + dtb_base, dtb_size, env_base);
        */

        // Initialize RMM
        crate::arch::rmm::init(
            args.kernel_base, args.kernel_size,
            args.stack_base, args.stack_size,
            args.env_base, args.env_size,
            args.dtb_base, args.dtb_size,
            args.areas_base, args.areas_size,
            args.bootstrap_base, args.bootstrap_size,
        );

        // Initialize paging
        let tcb_offset = paging::init(0);

        // Test tdata and tbss
        {
            assert_eq!(TBSS_TEST_ZERO, 0);
            TBSS_TEST_ZERO += 1;
            assert_eq!(TBSS_TEST_ZERO, 1);
            assert_eq!(TDATA_TEST_NONZERO, 0xFFFF_FFFF_FFFF_FFFF);
            TDATA_TEST_NONZERO -= 1;
            assert_eq!(TDATA_TEST_NONZERO, 0xFFFF_FFFF_FFFF_FFFE);
        }

        // Reset AP variables
        CPU_COUNT.store(1, Ordering::SeqCst);
        AP_READY.store(false, Ordering::SeqCst);
        BSP_READY.store(false, Ordering::SeqCst);

        // Setup kernel heap
        allocator::init();

        // Set up double buffer for grpahical debug now that heap is available
        #[cfg(feature = "graphical_debug")]
        graphical_debug::init_heap();

        // Activate memory logging
        log::init();

        // Initialize devices
        device::init();

        // Initialize all of the non-core devices not otherwise needed to complete initialization
        device::init_noncore();

        // Stop graphical debug
        #[cfg(feature = "graphical_debug")]
        graphical_debug::fini();

        BSP_READY.store(true, Ordering::SeqCst);

        crate::Bootstrap {
            base: crate::memory::Frame::containing_address(crate::paging::PhysicalAddress::new(args.bootstrap_base)),
            page_count: args.bootstrap_size / crate::memory::PAGE_SIZE,
            entry: args.bootstrap_entry,
            env,
        }
    };

    crate::kmain(CPU_COUNT.load(Ordering::SeqCst), bootstrap);
}

#[repr(packed)]
pub struct KernelArgsAp {
    cpu_id: u64,
    page_table: u64,
    stack_start: u64,
    stack_end: u64,
}

/// Entry to rust for an AP
pub unsafe extern fn kstart_ap(args_ptr: *const KernelArgsAp) -> ! {
    loop{}
}

#[naked]
//TODO: AbiCompatBool
//TODO: clear all regs?
pub unsafe extern "C" fn usermode(_ip: usize, _sp: usize, _arg: usize, _is_singlestep: usize) -> ! {
    core::arch::asm!(
        "
        msr spsr_el1, xzr // spsr
        msr elr_el1, x0 // ip
        msr sp_el0, x1 // sp
        mov x0, x2 // arg
        eret
        ",
        options(noreturn),
    );
}
