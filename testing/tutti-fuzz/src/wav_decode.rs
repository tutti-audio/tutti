/// Decodes `data` as a file; an error is an answer, a panic is a finding.
pub fn wav_decode(data: &[u8]) {
    let _ = tutti_codec::read_wave::<f32>(data.to_vec());
}
