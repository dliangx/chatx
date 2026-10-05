use webrtc_rs::track::track_remote::TrackRemote;

use crate::error::{Error, Result};
use crate::signal::SignalFrame;

/// Decode one in-flight H.264 Annex-B NAL stream (a full frame accumulated by
/// the receive loop) into RGBA8888.
pub(crate) fn h264_decode_annexb(nal: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    use openh264::decoder::Decoder;
    use openh264::nal_units;

    let mut decoder = Decoder::new().map_err(Error::OpenH264)?;
    let mut last: Option<(Vec<u8>, u32, u32)> = None;
    // Re-emit the latest successful decode if the NAL stream contains multiple
    // (SPS/PPS + frame); keep the last one.
    for packet in nal_units(nal) {
        if let Ok(yuv) = decoder.decode(packet) {
            let w = yuv.width();
            let h = yuv.height();
            let rgba: Vec<u8> = yuv.to_rgba8();
            last = Some((rgba, w, h));
        }
    }
    let Some(out) = last else {
        return Err(Error::other("openh264 decode: no frames"));
    };
    Ok(out)
}

// Re-export the trait for downstream `use media::...` convenience.
#[allow(unused_imports)]
use TrackRemote as _TrackRemote;
