use core::{ascii, mem};
use alloc::string::String;
use alloc::vec::Vec;

use super::data::{Map, Stat, TimeSpec};
use super::{flag::*, copy_path_to_buf};
use super::number::*;
use super::usercopy::UserSlice;

use crate::syscall::error::Result;

struct ByteStr<'a>(&'a[u8]);

impl<'a> ::core::fmt::Debug for ByteStr<'a> {
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        write!(f, "\"")?;
        for i in self.0 {
            for ch in ascii::escape_default(*i) {
                write!(f, "{}", ch as char)?;
            }
        }
        write!(f, "\"")?;
        Ok(())
    }
}
fn debug_path(ptr: usize, len: usize) -> Result<String> {
    // TODO: PATH_MAX
    UserSlice::ro(ptr, len).and_then(|slice| copy_path_to_buf(slice, 4096))
}
fn debug_buf(ptr: usize, len: usize) -> Result<Vec<u8>> {
    UserSlice::ro(ptr, len).and_then(|user| {
        let mut buf = vec! [0_u8; 4096];
        let count = user.copy_common_bytes_to_slice(&mut buf)?;
        buf.truncate(count);
        Ok(buf)
    })
}
unsafe fn read_struct<T>(ptr: usize) -> Result<T> {
    UserSlice::ro(ptr, mem::size_of::<T>()).and_then(|slice| slice.read_exact::<T>())
}

//TODO: calling format_call with arguments from another process space will not work
pub fn format_call(a: usize, b: usize, c: usize, d: usize, e: usize, f: usize) -> String {
    match a {
        SYS_OPEN => format!(
            "open({:?}, {:#X})",
            debug_path(b, c).as_ref().map(|p| ByteStr(p.as_bytes())),
            d
        ),
        SYS_RMDIR => format!(
            "rmdir({:?})",
            debug_path(b, c).as_ref().map(|p| ByteStr(p.as_bytes())),
        ),
        SYS_UNLINK => format!(
            "unlink({:?})",
            debug_path(b, c).as_ref().map(|p| ByteStr(p.as_bytes())),
        ),
        SYS_CLOSE => format!(
            "close({})", b
        ),
        SYS_DUP => format!(
            "dup({}, {:?})",
            b,
            debug_buf(c, d).as_ref().map(|b| ByteStr(&*b)),
        ),
        SYS_DUP2 => format!(
            "dup2({}, {}, {:?})",
            b,
            c,
            debug_buf(d, e).as_ref().map(|b| ByteStr(&*b)),
        ),
        SYS_READ => format!(
            "read({}, {:#X}, {})",
            b,
            c,
            d
        ),
        SYS_WRITE => format!(
            "write({}, {:#X}, {})",
            b,
            c,
            d
        ),
        SYS_LSEEK => format!(
            "lseek({}, {}, {} ({}))",
            b,
            c as isize,
            match d {
                SEEK_SET => "SEEK_SET",
                SEEK_CUR => "SEEK_CUR",
                SEEK_END => "SEEK_END",
                _ => "UNKNOWN"
            },
            d
        ),
        SYS_FCHMOD => format!(
            "fchmod({}, {:#o})",
            b,
            c
        ),
        SYS_FCHOWN => format!(
            "fchown({}, {}, {})",
            b,
            c,
            d
        ),
        SYS_FCNTL => format!(
            "fcntl({}, {} ({}), {:#X})",
            b,
            match c {
                F_DUPFD => "F_DUPFD",
                F_GETFD => "F_GETFD",
                F_SETFD => "F_SETFD",
                F_SETFL => "F_SETFL",
                F_GETFL => "F_GETFL",
                _ => "UNKNOWN"
            },
            c,
            d
        ),
        SYS_FMAP => format!(
            "fmap({}, {:?})",
            b,
            UserSlice::ro(c, d).and_then(|buf| unsafe { buf.read_exact::<Map>() }),
        ),
        SYS_FUNMAP => format!(
            "funmap({:#X}, {:#X})",
            b,
            c,
        ),
        SYS_FPATH => format!(
            "fpath({}, {:#X}, {})",
            b,
            c,
            d
        ),
        SYS_FRENAME => format!(
            "frename({}, {:?})",
            b,
            debug_path(c, d),
        ),
        SYS_FSTAT => format!(
            "fstat({}, {:?})",
            b,
            UserSlice::ro(c, d).and_then(|buf| unsafe { buf.read_exact::<Stat>() }),
        ),
        SYS_FSTATVFS => format!(
            "fstatvfs({}, {:#X}, {})",
            b,
            c,
            d
        ),
        SYS_FSYNC => format!(
            "fsync({})",
            b
        ),
        SYS_FTRUNCATE => format!(
            "ftruncate({}, {})",
            b,
            c
        ),
        SYS_FUTIMENS => format!(
            "futimens({}, {:?})",
            b,
            UserSlice::ro(c, d).and_then(|buf| {
                let mut times = vec! [unsafe { buf.read_exact::<TimeSpec>()? }];

                // One or two timespecs
                if let Some(second) = buf.advance(mem::size_of::<TimeSpec>()) {
                    times.push(unsafe { second.read_exact::<TimeSpec>()? });
                }
                Ok(times)
            }),
        ),

        SYS_CLOCK_GETTIME => format!(
            "clock_gettime({}, {:?})",
            b,
            unsafe { read_struct::<TimeSpec>(c) }
        ),
        SYS_EXIT => format!(
            "exit({})",
            b
        ),
        SYS_FUTEX => format!(
            "futex({:#X} [{:?}], {}, {}, {}, {})",
            b,
            UserSlice::ro(b, 4).and_then(|buf| buf.read_u32()),
            c,
            d,
            e,
            f
        ),
        SYS_GETEGID => format!("getegid()"),
        SYS_GETENS => format!("getens()"),
        SYS_GETEUID => format!("geteuid()"),
        SYS_GETGID => format!("getgid()"),
        SYS_GETNS => format!("getns()"),
        SYS_GETPGID => format!("getpgid()"),
        SYS_GETPID => format!("getpid()"),
        SYS_GETPPID => format!("getppid()"),
        SYS_GETUID => format!("getuid()"),
        SYS_IOPL => format!(
            "iopl({})",
            b
        ),
        SYS_KILL => format!(
            "kill({}, {})",
            b,
            c
        ),
        SYS_SIGRETURN => format!("sigreturn()"),
        SYS_SIGACTION => format!(
            "sigaction({}, {:#X}, {:#X}, {:#X})",
            b,
            c,
            d,
            e
        ),
        SYS_SIGPROCMASK => format!(
            "sigprocmask({}, {:?}, {:?})",
            b,
            unsafe { read_struct::<[u64; 2]>(c) },
            unsafe { read_struct::<[u64; 2]>(d) },
        ),
        SYS_MKNS => format!(
            "mkns({:p} len: {})",
            // TODO: Print out all scheme names?

            // Simply printing out simply the pointers and lengths may not provide that much useful
            // debugging information, so only print the raw args.
            b as *const u8,
            c,
        ),
        SYS_MPROTECT => format!(
            "mprotect({:#X}, {}, {:?})",
            b,
            c,
            MapFlags::from_bits(d)
        ),
        SYS_NANOSLEEP => format!(
            "nanosleep({:?}, ({}, {}))",
            unsafe { read_struct::<TimeSpec>(b) },
            c,
            d
        ),
        SYS_PHYSALLOC => format!(
            "physalloc({})",
            b
        ),
        SYS_PHYSALLOC3 => format!(
            "physalloc3({}, {}, {})",
            b, c, d,
        ),
        SYS_PHYSFREE => format!(
            "physfree({:#X}, {})",
            b,
            c
        ),
        SYS_PHYSMAP => format!(
            "physmap({:#X}, {}, {:?})",
            b,
            c,
            PhysmapFlags::from_bits(d)
        ),
        SYS_VIRTTOPHYS => format!(
            "virttophys({:#X})",
            b
        ),
        SYS_PIPE2 => format!(
            "pipe2({:?}, {})",
            unsafe { read_struct::<[usize; 2]>(b) },
            c
        ),
        SYS_SETREGID => format!(
            "setregid({}, {})",
            b,
            c
        ),
        SYS_SETRENS => format!(
            "setrens({}, {})",
            b,
            c
        ),
        SYS_SETREUID => format!(
            "setreuid({}, {})",
            b,
            c
        ),
        SYS_UMASK => format!(
            "umask({:#o}",
            b
        ),
        SYS_WAITPID => format!(
            "waitpid({}, {:#X}, {:?})",
            b,
            c,
            WaitFlags::from_bits(d)
        ),
        SYS_YIELD => format!("yield()"),
        _ => format!(
            "UNKNOWN{} {:#X}({:#X}, {:#X}, {:#X}, {:#X}, {:#X})",
            a, a,
            b,
            c,
            d,
            e,
            f
        )
    }
}
