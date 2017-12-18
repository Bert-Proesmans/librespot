use std::io;
use std::mem;
use std::slice;

use super::Sink;

pub struct DynamicSink<W>
where for<'r> W: FnMut(&'r [u8]) -> io::Result<()> + Send + 'static
{
	pub(crate) writer: W
}

impl<W> Sink for DynamicSink<W> 
where for<'r> W: FnMut(&'r [u8]) -> io::Result<()> + Send + 'static
{
	fn start(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn stop(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn write(&mut self, data: &[i16]) -> io::Result<()> {
        let data: &[u8] = unsafe {
            slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * mem::size_of::<i16>())
        };

        (self.writer)(data)
    }
}
