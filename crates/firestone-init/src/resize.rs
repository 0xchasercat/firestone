//! Deciding whether — and how far — to grow the root filesystem.
//!
//! SPEC §10.5 step 3: `/dev/vda` already presents the machine's full virtual
//! size, so the root ext4 is resized online to `disk_size_bytes` divided by the
//! filesystem block size. The arithmetic is separated from the ioctl so it can
//! be unit-tested without a Linux guest.

/// Returns the block count to pass to `EXT4_IOC_RESIZE_FS`, or `None` when the
/// filesystem already covers the device.
///
/// The target never exceeds what the block device actually offers, so a config
/// document that names a larger disk than the VMM attached cannot ask the
/// kernel to grow past the end of the device.
#[must_use]
pub fn resize_target_blocks(
    requested_bytes: u64,
    device_bytes: u64,
    block_size: u64,
    current_blocks: u64,
) -> Option<u64> {
    if block_size == 0 {
        return None;
    }
    let capped = requested_bytes.min(device_bytes);
    let target = capped / block_size;
    (target > current_blocks).then_some(target)
}

#[cfg(test)]
mod tests {
    use super::resize_target_blocks;

    const BLOCK: u64 = 4096;

    #[test]
    fn resize_target_blocks_grows_to_the_requested_size() {
        assert_eq!(
            resize_target_blocks(20 * 1024 * 1024, 20 * 1024 * 1024, BLOCK, 1024),
            Some(5120)
        );
    }

    #[test]
    fn resize_target_blocks_is_capped_by_the_device() {
        assert_eq!(
            resize_target_blocks(64 * 1024 * 1024, 8 * 1024 * 1024, BLOCK, 512),
            Some(2048)
        );
    }

    #[test]
    fn resize_target_blocks_filesystem_already_at_size_is_a_no_op() {
        assert_eq!(resize_target_blocks(4 * BLOCK, 4 * BLOCK, BLOCK, 4), None);
    }

    #[test]
    fn resize_target_blocks_filesystem_larger_than_request_is_a_no_op() {
        assert_eq!(resize_target_blocks(BLOCK, 64 * BLOCK, BLOCK, 16), None);
    }

    #[test]
    fn resize_target_blocks_zero_block_size_is_refused() {
        assert_eq!(resize_target_blocks(1024, 1024, 0, 0), None);
    }
}
