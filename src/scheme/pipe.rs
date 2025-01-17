use core::sync::atomic::{AtomicUsize, Ordering, AtomicBool};

use alloc::sync::Arc;
use alloc::collections::{BTreeMap, VecDeque};

use spin::{Mutex, Once, RwLock};

use crate::event;
use crate::scheme::SchemeId;
use crate::sync::WaitCondition;
use crate::syscall::error::{Error, Result, EAGAIN, EBADF, EINTR, EINVAL, ENOENT, EPIPE, ESPIPE};
use crate::syscall::flag::{EventFlags, EVENT_READ, EVENT_WRITE, F_GETFL, F_SETFL, O_ACCMODE, O_NONBLOCK, MODE_FIFO};
use crate::syscall::scheme::{CallerCtx, Scheme};
use crate::syscall::data::Stat;
use crate::syscall::usercopy::{UserSliceWo, UserSliceRo};

use super::{KernelScheme, OpenResult};

// TODO: Preallocate a number of scheme IDs, since there can only be *one* root namespace, and
// therefore only *one* pipe scheme.
static THE_PIPE_SCHEME: Once<(SchemeId, Arc<dyn KernelScheme>)> = Once::new();
static PIPE_NEXT_ID: AtomicUsize = AtomicUsize::new(1);

// TODO: SLOB?
static PIPES: RwLock<BTreeMap<usize, Arc<Pipe>>> = RwLock::new(BTreeMap::new());

pub fn pipe_scheme_id() -> SchemeId {
    THE_PIPE_SCHEME.get().expect("pipe scheme must be initialized").0
}

const MAX_QUEUE_SIZE: usize = 65536;

// In almost all places where Rust (and LLVM) uses pointers, they are limited to nonnegative isize,
// so this is fine.
const WRITE_NOT_READ_BIT: usize = 1 << (usize::BITS - 1);

fn from_raw_id(id: usize) -> (bool, usize) {
    (id & WRITE_NOT_READ_BIT != 0, id & !WRITE_NOT_READ_BIT)
}

pub fn pipe(flags: usize) -> Result<(usize, usize)> {
    let id = PIPE_NEXT_ID.fetch_add(1, Ordering::Relaxed);

    PIPES.write().insert(id, Arc::new(Pipe {
        read_flags: AtomicUsize::new(flags),
        write_flags: AtomicUsize::new(flags),
        queue: Mutex::new(VecDeque::new()),
        read_condition: WaitCondition::new(),
        write_condition: WaitCondition::new(),
        writer_is_alive: AtomicBool::new(true),
        reader_is_alive: AtomicBool::new(true),
        has_run_dup: AtomicBool::new(false),
    }));

    Ok((id, id | WRITE_NOT_READ_BIT))
}

pub struct PipeScheme;

impl PipeScheme {
    pub fn new(scheme_id: SchemeId) -> Arc<dyn KernelScheme> {
        Arc::clone(&THE_PIPE_SCHEME.call_once(|| {
            (scheme_id, Arc::new(Self))
        }).1)
    }
}

impl Scheme for PipeScheme {

    fn fcntl(&self, id: usize, cmd: usize, arg: usize) -> Result<usize> {
        let (is_writer_not_reader, key) = from_raw_id(id);
        let pipe = Arc::clone(PIPES.read().get(&key).ok_or(Error::new(EBADF))?);

        let flags = if is_writer_not_reader { &pipe.write_flags } else { &pipe.read_flags };

        match cmd {
            F_GETFL => Ok(flags.load(Ordering::SeqCst)),
            F_SETFL => {
                flags.store(arg & !O_ACCMODE, Ordering::SeqCst);
                Ok(0)
            },
            _ => Err(Error::new(EINVAL))
        }
    }

    fn fevent(&self, id: usize, flags: EventFlags) -> Result<EventFlags> {
        let (is_writer_not_reader, key) = from_raw_id(id);
        let pipe = Arc::clone(PIPES.read().get(&key).ok_or(Error::new(EBADF))?);

        if is_writer_not_reader && flags == EVENT_WRITE {
            // TODO: Return correct flags
            if pipe.queue.lock().len() >= MAX_QUEUE_SIZE {
                return Ok(EventFlags::empty());
            } else {
                return Ok(EVENT_WRITE);
            }
        } else if flags == EVENT_READ {
            // TODO: Return correct flags
            if pipe.queue.lock().is_empty() {
                return Ok(EventFlags::empty());
            } else {
                return Ok(EVENT_READ);
            }
        }

        Err(Error::new(EBADF))
    }

    fn fsync(&self, _id: usize) -> Result<usize> {
        Ok(0)
    }

    fn close(&self, id: usize) -> Result<usize> {
        let (is_write_not_read, key) = from_raw_id(id);

        let pipe = Arc::clone(PIPES.read().get(&key).ok_or(Error::new(EBADF))?);
        let scheme_id = pipe_scheme_id();

        let can_remove = if is_write_not_read {
            event::trigger(scheme_id, key, EVENT_READ);

            pipe.read_condition.notify();
            pipe.writer_is_alive.store(false, Ordering::SeqCst);

            !pipe.reader_is_alive.load(Ordering::SeqCst)
        } else {
            event::trigger(scheme_id, key | WRITE_NOT_READ_BIT, EVENT_WRITE);

            pipe.write_condition.notify();
            pipe.reader_is_alive.store(false, Ordering::SeqCst);

            !pipe.writer_is_alive.load(Ordering::SeqCst)
        };

        if can_remove {
            let _ = PIPES.write().remove(&key);
        }

        Ok(0)
    }

    fn seek(&self, _id: usize, _pos: isize, _whence: usize) -> Result<isize> {
        Err(Error::new(ESPIPE))
    }
}

pub struct Pipe {
    read_flags: AtomicUsize, // fcntl read flags
    write_flags: AtomicUsize, // fcntl write flags
    read_condition: WaitCondition, // signals whether there are available bytes to read
    write_condition: WaitCondition, // signals whether there is room for additional bytes
    queue: Mutex<VecDeque<u8>>,
    reader_is_alive: AtomicBool, // starts set, unset when reader closes
    writer_is_alive: AtomicBool, // starts set, unset when writer closes
    has_run_dup: AtomicBool,
}

impl KernelScheme for PipeScheme {
    fn kdup(&self, old_id: usize, user_buf: UserSliceRo, _ctx: CallerCtx) -> Result<OpenResult> {
        let (is_writer_not_reader, key) = from_raw_id(old_id);

        if is_writer_not_reader {
            return Err(Error::new(EBADF));
        }

        let mut buf = [0_u8; 5];

        if user_buf.copy_common_bytes_to_slice(&mut buf)? < 5 || buf != *b"write" {
            return Err(Error::new(EINVAL));
        }

        let pipe = Arc::clone(PIPES.read().get(&key).ok_or(Error::new(EBADF))?);

        if pipe.has_run_dup.swap(true, Ordering::SeqCst) {
            return Err(Error::new(EBADF));
        }

        Ok(OpenResult::SchemeLocal(key | WRITE_NOT_READ_BIT))
    }
    fn kopen(&self, path: &str, flags: usize, _ctx: CallerCtx) -> Result<OpenResult> {
        if !path.trim_start_matches('/').is_empty() {
            return Err(Error::new(ENOENT));
        }

        let (read_id, _) = pipe(flags)?;

        Ok(OpenResult::SchemeLocal(read_id))
    }

    fn kread(&self, id: usize, user_buf: UserSliceWo) -> Result<usize> {
        let (is_write_not_read, key) = from_raw_id(id);

        if is_write_not_read {
            return Err(Error::new(EBADF));
        }
        let pipe = Arc::clone(PIPES.read().get(&key).ok_or(Error::new(EBADF))?);

        loop {
            let mut vec = pipe.queue.lock();

            let (s1, s2) = vec.as_slices();
            let s1_count = core::cmp::min(user_buf.len(), s1.len());

            let (s1_dst, s2_buf) = user_buf.split_at(s1_count).expect("s1_count <= user_buf.len()");
            s1_dst.copy_from_slice(&s1[..s1_count])?;

            let s2_count = core::cmp::min(s2_buf.len(), s2.len());
            s2_buf.limit(s2_count).expect("s2_count <= s2_buf.len()").copy_from_slice(&s2[..s2_count])?;

            let bytes_read = s1_count + s2_count;
            let _ = vec.drain(..bytes_read);

            if bytes_read > 0 {
                event::trigger(pipe_scheme_id(), key | WRITE_NOT_READ_BIT, EVENT_WRITE);
                pipe.write_condition.notify();

                return Ok(bytes_read);
            } else if user_buf.is_empty() {
                return Ok(0);
            }

            if !pipe.writer_is_alive.load(Ordering::SeqCst) {
                return Ok(0);
            } else if pipe.read_flags.load(Ordering::SeqCst) & O_NONBLOCK == O_NONBLOCK {
                return Err(Error::new(EAGAIN));
            } else if !pipe.read_condition.wait(vec, "PipeRead::read") {
                return Err(Error::new(EINTR));
            }
        }
    }
    fn kwrite(&self, id: usize, user_buf: UserSliceRo) -> Result<usize> {
        let (is_write_not_read, key) = from_raw_id(id);

        if !is_write_not_read {
            return Err(Error::new(EBADF));
        }
        let pipe = Arc::clone(PIPES.read().get(&key).ok_or(Error::new(EBADF))?);

        loop {
            let mut vec = pipe.queue.lock();

            let bytes_left = MAX_QUEUE_SIZE.saturating_sub(vec.len());
            let bytes_to_write = core::cmp::min(bytes_left, user_buf.len());
            let src_buf = user_buf.limit(bytes_to_write).expect("bytes_to_write <= user_buf.len()");

            const TMPBUF_SIZE: usize = 512;
            let mut tmp_buf = [0_u8; TMPBUF_SIZE];

            let mut bytes_written = 0;

            // TODO: Modify VecDeque so that the unwritten portions can be accessed directly?
            for (idx, chunk) in src_buf.in_variable_chunks(TMPBUF_SIZE).enumerate() {
                let chunk_byte_count = match chunk.copy_common_bytes_to_slice(&mut tmp_buf) {
                    Ok(c) => c,
                    Err(_) if idx > 0 => break,
                    Err(error) => return Err(error),
                };
                vec.extend(&tmp_buf[..chunk_byte_count]);
                bytes_written += chunk_byte_count;
            }

            if bytes_written > 0 {
                event::trigger(pipe_scheme_id(), key, EVENT_READ);
                pipe.read_condition.notify();

                return Ok(bytes_written);
            } else if user_buf.is_empty() {
                return Ok(0);
            }

            if !pipe.reader_is_alive.load(Ordering::SeqCst) {
                return Err(Error::new(EPIPE));
            } else if pipe.write_flags.load(Ordering::SeqCst) & O_NONBLOCK == O_NONBLOCK {
                return Err(Error::new(EAGAIN));
            } else if !pipe.write_condition.wait(vec, "PipeWrite::write") {
                return Err(Error::new(EINTR));
            }
        }
    }
    fn kfstat(&self, _id: usize, buf: UserSliceWo) -> Result<usize> {
        buf.copy_exactly(&Stat {
            st_mode: MODE_FIFO | 0o666,
            ..Default::default()
        })?;

        Ok(0)
    }
}
