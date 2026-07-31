#[cfg(windows)]
fn main() {
    use std::mem::zeroed;

    use windows_sys::Win32::Storage::FileSystem::{
        CloseIoRing, CreateIoRing, IsIoRingOpSupported, QueryIoRingCapabilities,
        IORING_CAPABILITIES, IORING_CREATE_ADVISORY_FLAGS_NONE, IORING_CREATE_FLAGS,
        IORING_CREATE_REQUIRED_FLAGS_NONE, IORING_OP_CANCEL, IORING_OP_FLUSH, IORING_OP_NOP,
        IORING_OP_READ, IORING_OP_REGISTER_BUFFERS, IORING_OP_REGISTER_FILES, IORING_OP_WRITE,
        IORING_VERSION_1, IORING_VERSION_2, IORING_VERSION_3,
    };

    unsafe {
        let mut caps: IORING_CAPABILITIES = zeroed();
        let hr = QueryIoRingCapabilities(&mut caps);
        println!("QueryIoRingCapabilities hr=0x{:08x}", hr as u32);
        if hr < 0 {
            return;
        }

        println!("MaxVersion={}", caps.MaxVersion);
        println!("MaxSubmissionQueueSize={}", caps.MaxSubmissionQueueSize);
        println!("MaxCompletionQueueSize={}", caps.MaxCompletionQueueSize);
        println!("FeatureFlags=0x{:x}", caps.FeatureFlags);

        let version = if caps.MaxVersion >= IORING_VERSION_3 {
            IORING_VERSION_3
        } else if caps.MaxVersion >= IORING_VERSION_2 {
            IORING_VERSION_2
        } else {
            IORING_VERSION_1
        };
        let flags = IORING_CREATE_FLAGS {
            Required: IORING_CREATE_REQUIRED_FLAGS_NONE,
            Advisory: IORING_CREATE_ADVISORY_FLAGS_NONE,
        };
        let mut ring = std::ptr::null_mut();
        let hr = CreateIoRing(version, flags, 256, 256, &mut ring);
        println!("CreateIoRing version={} hr=0x{:08x}", version, hr as u32);
        if hr < 0 {
            return;
        }

        for (name, op) in [
            ("nop", IORING_OP_NOP),
            ("read", IORING_OP_READ),
            ("register_files", IORING_OP_REGISTER_FILES),
            ("register_buffers", IORING_OP_REGISTER_BUFFERS),
            ("cancel", IORING_OP_CANCEL),
            ("write", IORING_OP_WRITE),
            ("flush", IORING_OP_FLUSH),
        ] {
            println!(
                "op_supported[{name}]={}",
                IsIoRingOpSupported(ring, op) != 0
            );
        }

        let hr = CloseIoRing(ring);
        println!("CloseIoRing hr=0x{:08x}", hr as u32);
    }
}

#[cfg(not(windows))]
fn main() {
    println!("ioring_probe is only meaningful on Windows");
}
