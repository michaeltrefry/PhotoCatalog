use std::io::{self,Write,Seek,SeekFrom};
/// Limits the maximum file extent, including forward seeks. Rewrites of TIFF
/// directory offsets do not spend the allowance a second time. The caller must
/// supply a newly created, empty staging sink and retain it on failure.
pub struct BoundedSeekWriter<W> { inner:W,limit:u64,position:u64,extent:u64,exceeded:bool }
impl<W:Write+Seek> BoundedSeekWriter<W> {
    pub fn new(mut inner:W,limit:u64)->io::Result<Self> {
        if limit==0 || inner.seek(SeekFrom::End(0))?!=0 {return Err(io::Error::new(io::ErrorKind::InvalidInput,"nonzero limit and empty staging sink required"));}
        inner.seek(SeekFrom::Start(0))?;Ok(Self{inner,limit,position:0,extent:0,exceeded:false})
    }
    pub fn extent(&self)->u64 {self.extent}
    pub fn limit(&self)->u64 {self.limit}
    pub fn into_inner(self)->W {self.inner}
    pub(crate) fn exceeded(&self)->bool {self.exceeded}
    fn admitted(&mut self,n:u64)->io::Result<u64> {if n>self.limit {self.exceeded=true;Err(io::Error::new(io::ErrorKind::StorageFull,"encoded output extent limit exceeded"))}else{Ok(n)}}
}
impl<W:Write+Seek> Write for BoundedSeekWriter<W> {
    fn write(&mut self,buf:&[u8])->io::Result<usize> {
        let end=self.position.checked_add(buf.len() as u64).ok_or_else(||io::Error::new(io::ErrorKind::StorageFull,"output extent overflow"))?;self.admitted(end)?;
        let n=self.inner.write(buf)?;self.position+=n as u64;self.extent=self.extent.max(self.position);Ok(n)
    }
    fn flush(&mut self)->io::Result<()> {self.inner.flush()}
}
impl<W:Write+Seek> Seek for BoundedSeekWriter<W> {
    fn seek(&mut self,to:SeekFrom)->io::Result<u64> {
        let next=match to {SeekFrom::Start(n)=>Some(n),SeekFrom::Current(n)=>self.position.checked_add_signed(n),SeekFrom::End(n)=>self.extent.checked_add_signed(n)}.ok_or_else(||io::Error::new(io::ErrorKind::InvalidInput,"invalid bounded seek"))?;
        self.admitted(next)?;self.position=self.inner.seek(SeekFrom::Start(next))?;Ok(self.position)
    }
}
#[cfg(test)] mod tests {use super::*;
#[test]fn extent_counts_rewrites_once_and_rejects_forward_holes(){let mut w=BoundedSeekWriter::new(io::Cursor::new(Vec::new()),8).unwrap();w.write_all(b"12345678").unwrap();w.seek(SeekFrom::Start(0)).unwrap();w.write_all(b"ab").unwrap();assert_eq!(w.extent(),8);assert!(w.seek(SeekFrom::Start(9)).is_err());w.seek(SeekFrom::End(0)).unwrap();assert!(w.write_all(b"!").is_err());assert_eq!(w.into_inner().into_inner(),b"ab345678");}
}
