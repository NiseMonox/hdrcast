//! Sizes libsrt's flow-control window and socket buffers for a given latency x bitrate product.
//!
//! At >~40 Mbps with >=2 s latency the stream in flight overflows libsrt's default 8192-packet receive buffer
//! ("No room to store incoming packet" then "SEQUENCE DISCREPANCY"), so both ends put these values in the
//! srt:// URL (docs/research/srt-mediamtx.research.json).

/// SRT payload per packet (7 x 188-byte TS packets).
pub const PAYLOAD_BYTES: u32 = 1316;
/// Bytes libsrt reserves per buffered packet (MSS 1500 - 28).
pub const PACKET_SLOT_BYTES: u32 = 1472;
/// libsrt default SRTO_FC.
pub const DEFAULT_FLOW_WINDOW: u32 = 25600;
/// UDP socket receive buffer (recv_buffer_size=) recommended for readers.
pub const RECV_SOCKET_BUFFER_BYTES: u32 = 8_000_000;

/// Result of [`compute`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SrtBufferPlan {
    /// Packets in flight for latency x bitrate (ceil(latency_s * bitrate_bps / (1316*8))).
    pub packets: u32,
    /// SRTO_FC (ffs=) value: max(25600, next power of two >= packets * 1.2).
    pub flow_window: u32,
    /// sndbuf= / rcvbuf= value in bytes: flow_window * 1472 (SRTO_RCVBUF must be <= FC * (MSS-28)).
    pub buffer_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SrtBufferSizingError {
    #[error("latency must be positive")]
    Latency,
    #[error("bitrate must be positive")]
    Bitrate,
    /// The flow window would not fit libsrt's int (latency x bitrate far beyond anything real).
    #[error("latency x bitrate is too large for an SRT flow window")]
    TooLarge,
}

pub fn compute(latency_ms: u32, bitrate_bps: u64) -> Result<SrtBufferPlan, SrtBufferSizingError> {
    if latency_ms == 0 {
        return Err(SrtBufferSizingError::Latency);
    }
    if bitrate_bps == 0 {
        return Err(SrtBufferSizingError::Bitrate);
    }

    let latency_seconds = f64::from(latency_ms) / 1000.0;
    let packets_exact = latency_seconds * bitrate_bps as f64 / (f64::from(PAYLOAD_BYTES) * 8.0);
    if packets_exact > f64::from(i32::MAX) {
        return Err(SrtBufferSizingError::TooLarge);
    }
    let packets = (packets_exact.ceil() as u32).max(1);

    let target = (f64::from(packets) * 1.2).ceil() as u64;
    let flow_window =
        if target <= u64::from(DEFAULT_FLOW_WINDOW) { DEFAULT_FLOW_WINDOW } else { next_power_of_two(target)? };
    let buffer_bytes = u64::from(flow_window) * u64::from(PACKET_SLOT_BYTES);
    Ok(SrtBufferPlan { packets, flow_window, buffer_bytes })
}

/// Smallest power of two that is >= value; an error when it does not fit a C int (SRTO_FC is one).
pub fn next_power_of_two(value: u64) -> Result<u32, SrtBufferSizingError> {
    if value <= 1 {
        return Ok(1);
    }
    match value.checked_next_power_of_two() {
        Some(p) if p <= i32::MAX as u64 => Ok(p as u32),
        _ => Err(SrtBufferSizingError::TooLarge),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powers_of_two_round_up() {
        assert_eq!(next_power_of_two(0), Ok(1));
        assert_eq!(next_power_of_two(1), Ok(1));
        assert_eq!(next_power_of_two(25601), Ok(32768));
        assert_eq!(next_power_of_two(32768), Ok(32768));
        assert_eq!(next_power_of_two(1 << 30), Ok(1 << 30));
        assert_eq!(next_power_of_two((1 << 30) + 1), Err(SrtBufferSizingError::TooLarge));
    }

    #[test]
    fn zero_inputs_and_absurd_products_are_errors_not_panics() {
        assert_eq!(compute(0, 1), Err(SrtBufferSizingError::Latency));
        assert_eq!(compute(1, 0), Err(SrtBufferSizingError::Bitrate));
        assert_eq!(compute(u32::MAX, u64::MAX), Err(SrtBufferSizingError::TooLarge));
        assert_eq!(compute(1, 1).unwrap().packets, 1);
    }

    #[test]
    fn the_values_of_a_typical_stream() {
        // 40 Mbps at 1.5 s: 5700 packets, still inside libsrt's default window.
        let plan = compute(1500, 40_000_000).unwrap();
        assert_eq!(plan, SrtBufferPlan { packets: 5700, flow_window: 25600, buffer_bytes: 37_683_200 });
        // 100 Mbps at 3.5 s: 33245 packets * 1.2 = 39894 -> 65536.
        let big = compute(3500, 100_000_000).unwrap();
        assert_eq!((big.packets, big.flow_window, big.buffer_bytes), (33245, 65536, 96_468_992));
    }
}
