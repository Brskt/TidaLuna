use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex};

struct Inner {
    data: Vec<u8>,
    total_len: u64,
    finished: bool,
    cancelled: bool,
    error: Option<String>,
}

pub struct StreamingBuffer {
    inner: Arc<(Mutex<Inner>, Condvar)>,
    read_pos: u64,
}

pub struct StreamingBufferWriter {
    inner: Arc<(Mutex<Inner>, Condvar)>,
}

impl StreamingBuffer {
    pub fn new(total_len: u64) -> (Self, StreamingBufferWriter) {
        let inner = Arc::new((
            Mutex::new(Inner {
                data: Vec::with_capacity(total_len as usize),
                total_len,
                finished: false,
                cancelled: false,
                error: None,
            }),
            Condvar::new(),
        ));

        let buffer = StreamingBuffer {
            inner: inner.clone(),
            read_pos: 0,
        };

        let writer = StreamingBufferWriter { inner };

        (buffer, writer)
    }

    pub fn cancel(&self) {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();
        inner.cancelled = true;
        cvar.notify_all();
    }

    pub fn is_complete(&self) -> bool {
        let (lock, _) = &*self.inner;
        let inner = lock.lock().unwrap();
        inner.finished && inner.error.is_none()
    }

    pub fn total_len(&self) -> u64 {
        let (lock, _) = &*self.inner;
        let inner = lock.lock().unwrap();
        inner.total_len
    }

    #[cfg(target_os = "windows")]
    pub fn wait_for_complete(&self) {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();
        while !inner.finished && !inner.cancelled {
            inner = cvar.wait(inner).unwrap();
        }
    }

    pub fn to_vec(&self) -> Option<Vec<u8>> {
        let (lock, _) = &*self.inner;
        let inner = lock.lock().unwrap();
        if inner.finished && inner.error.is_none() {
            Some(inner.data.clone())
        } else {
            None
        }
    }

    pub fn new_reader(&self) -> Self {
        StreamingBuffer {
            inner: self.inner.clone(),
            read_pos: 0,
        }
    }
}

impl Read for StreamingBuffer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();

        loop {
            if inner.cancelled {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "streaming cancelled",
                ));
            }

            if let Some(ref err) = inner.error {
                return Err(io::Error::new(io::ErrorKind::Other, err.clone()));
            }

            let available = inner.data.len() as u64;
            if self.read_pos < available {
                let start = self.read_pos as usize;
                let end = std::cmp::min(start + buf.len(), inner.data.len());
                let n = end - start;
                buf[..n].copy_from_slice(&inner.data[start..end]);
                self.read_pos += n as u64;
                return Ok(n);
            }

            if inner.finished {
                return Ok(0);
            }

            inner = cvar.wait(inner).unwrap();
        }
    }
}

impl Seek for StreamingBuffer {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let (lock, _) = &*self.inner;
        let inner = lock.lock().unwrap();

        let new_pos = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => inner.total_len as i64 + offset,
            SeekFrom::Current(offset) => self.read_pos as i64 + offset,
        };

        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to negative position",
            ));
        }

        let new_pos = new_pos as u64;
        if new_pos > inner.total_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "seek beyond total length: {} > {}",
                    new_pos, inner.total_len
                ),
            ));
        }

        drop(inner);
        self.read_pos = new_pos;
        Ok(new_pos)
    }
}

impl StreamingBufferWriter {
    pub fn write(&self, data: &[u8]) {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();
        inner.data.extend_from_slice(data);
        cvar.notify_all();
    }

    pub fn finish(&self) {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();
        inner.finished = true;
        cvar.notify_all();
    }

    pub fn finish_with_error(&self, msg: String) {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap();
        inner.error = Some(msg);
        inner.finished = true;
        cvar.notify_all();
    }

    pub fn is_cancelled(&self) -> bool {
        let (lock, _) = &*self.inner;
        let inner = lock.lock().unwrap();
        inner.cancelled
    }
}
