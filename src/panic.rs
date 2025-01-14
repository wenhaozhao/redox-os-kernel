//! Intrinsics for panic handling

use core::alloc::Layout;
use core::panic::PanicInfo;

use crate::{cpu_id, context, interrupt, syscall};

/// Required to handle panics
#[panic_handler]
#[no_mangle]
pub extern "C" fn rust_begin_unwind(info: &PanicInfo) -> ! {
    println!("KERNEL PANIC: {}", info);

    unsafe { interrupt::stack_trace(); }

    println!("CPU {}, PID {:?}", cpu_id(), context::context_id());

    // This could deadlock, but at this point we are going to halt anyways
    {
        let contexts = context::contexts();
        if let Some(context_lock) = contexts.current() {
            let context = context_lock.read();
            println!("NAME: {}", context.name);

            if let Some((a, b, c, d, e, f)) = context.syscall {
                println!("SYSCALL: {}", syscall::debug::format_call(a, b, c, d, e, f));
            }
        }
    }

    println!("HALT");
    loop {
        unsafe { interrupt::halt(); }
    }
}

#[alloc_error_handler]
#[no_mangle]
#[allow(improper_ctypes_definitions)] // Layout is not repr(C)
pub extern fn rust_oom(_layout: Layout) -> ! {
    panic!("kernel memory allocation failed");
}
