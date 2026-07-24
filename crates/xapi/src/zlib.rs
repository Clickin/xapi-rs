use std::os::raw::{c_int, c_ulong};

const PREFIX: &[u8] = b"\xff\xad";
const OK: c_int = 0;
const BUF_ERROR: c_int = -5;

#[link(name = "z")]
unsafe extern "C" {
    fn uncompress(
        dest: *mut u8,
        dest_len: *mut c_ulong,
        source: *const u8,
        source_len: c_ulong,
    ) -> c_int;
    fn compress2(
        dest: *mut u8,
        dest_len: *mut c_ulong,
        source: *const u8,
        source_len: c_ulong,
        level: c_int,
    ) -> c_int;
}

pub fn is_wrapped(input: &[u8]) -> bool {
    input.starts_with(PREFIX)
}

pub fn inflate(input: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    if !is_wrapped(input) {
        return Err("missing PlatformZlib signature".into());
    }
    let mut size = (input.len().saturating_mul(4)).max(1024) as c_ulong;
    loop {
        let mut output = vec![0; size as usize];
        let mut written = size;
        let code = unsafe {
            uncompress(
                output.as_mut_ptr(),
                &mut written,
                input[2..].as_ptr(),
                (input.len() - 2) as c_ulong,
            )
        };
        if code == OK {
            output.truncate(written as usize);
            if output.len() > limit {
                return Err("decompressed payload exceeds limit".into());
            }
            return Ok(output);
        }
        if code != BUF_ERROR || size as usize >= limit {
            return Err("invalid PlatformZlib payload".into());
        }
        size = (size as usize * 2).min(limit.max(1)) as c_ulong;
    }
}

pub fn deflate(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut size = (input.len().saturating_mul(2)).max(64) as c_ulong;
    loop {
        let mut compressed = vec![0; size as usize];
        let mut written = size;
        let code = unsafe {
            compress2(
                compressed.as_mut_ptr(),
                &mut written,
                input.as_ptr(),
                input.len() as c_ulong,
                6,
            )
        };
        if code == OK {
            compressed.truncate(written as usize);
            let mut out = PREFIX.to_vec();
            out.extend(compressed);
            return Ok(out);
        }
        if code != BUF_ERROR {
            return Err("unable to compress PlatformZlib payload".into());
        }
        size *= 2;
    }
}
