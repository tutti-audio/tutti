use tutti_codec::Wave;

/// Decodes `data` as a file; an error is an answer, a panic is a finding.
pub fn wav_decode(data: &[u8]) {
    let _ = Wave::from_bytes(data.to_vec());
}
