//! MV-HEVC: multiview HEVC, two views of one scene in the layers of one
//! stream (Apple's spatial video from an iPhone or Vision Pro, Apple
//! Immersive Video, and other stereo cameras recording MV-HEVC). The base
//! layer is an ordinary HEVC picture, the second layer the other eye,
//! predicted from it.
//!
//! The Frame's hardware decoder knows only single-layer HEVC, so both views
//! are rewritten into one such stream ([`rewrite`]) that it decodes into the
//! very pictures of both ([`merged`]). Each pair is put side by side into
//! one picture twice as wide, which the renderer shows as full side-by-side
//! 3D.
//!
//! Without a hardware decoder, or for a stream the rewrite can't express, a
//! multiview stream decodes in software with FFmpeg's HEVC decoder, told to
//! give out every view: it gives each access unit's views one after the
//! other, with the same timestamp.
//!
//! A stream too large for either ([`plan`], Apple Immersive Video among
//! them) plays its base view alone as a 2D video, on the hardware decoder:
//! the layers above are taken out of its configuration and its packets
//! first, so the decoder sees plain HEVC.
//!
//! Which view is which eye comes from the stream when it says (its
//! three-dimensional reference displays SEI, exported on each frame), else
//! the base view is the left eye unless the container says the views are
//! reversed (the `stri` box of Apple's `vexu`).

use crate::decode::{Decoder, Frame, Packet};
use crate::input::Input;
use crate::mvc::side_by_side;
use crate::{Error, Result, check};
use fp_ffmpeg_sys as ff;

mod cabac;
pub(crate) mod merged;
mod rewrite;
mod syntax;

/// The NAL units of an `hvcC` decoder configuration record, in order; empty
/// when it isn't one.
pub(crate) fn record_units(record: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    if record.len() < 23 || record[0] != 1 {
        return out;
    }
    let mut rest = &record[23..];
    for _ in 0..record[22] {
        let [_, hi, lo, ref tail @ ..] = *rest else {
            return out;
        };
        rest = tail;
        for _ in 0..u16::from_be_bytes([hi, lo]) {
            let [hi, lo, ref tail @ ..] = *rest else {
                return out;
            };
            let len = usize::from(u16::from_be_bytes([hi, lo]));
            let Some(nal) = tail.get(..len) else {
                return out;
            };
            out.push(nal);
            rest = &tail[len..];
        }
    }
    out
}

/// Whether stream `index` of `input` is multiview HEVC: the demuxer found
/// an `lhvC` box (it then marks the stream multilayer), or the decoder
/// configuration holds parameter sets of a layer above the base.
pub(crate) fn is_multiview(input: &Input, index: usize) -> bool {
    let Some(&st) = input.streams().get(index) else {
        return false;
    };
    // SAFETY: the stream and its parameters live as long as the input.
    let (st, par) = unsafe { (&*st, &*(*st).codecpar) };
    if par.codec_id != ff::AV_CODEC_ID_HEVC || par.codec_type != ff::AVMEDIA_TYPE_VIDEO {
        return false;
    }
    let extradata = crate::mvc::extradata(par);
    let layered =
        st.disposition & ff::AV_DISPOSITION_MULTILAYER as i32 != 0 || hvcc_has_layers(extradata);
    layered && !other_scalability(extradata)
}

/// Whether a decoder configuration's VPS says its layers are something other
/// than views (HEVC with an alpha layer, or scalable): a video to play as
/// its base layer alone, like any HEVC.
fn other_scalability(extradata: &[u8]) -> bool {
    merged::configuration_units(extradata)
        .into_iter()
        .any(|nal| {
            syntax::NalHeader::parse(nal).is_some_and(|h| h.kind == syntax::VPS)
                && matches!(
                    syntax::vps(&crate::mvc::bits::unescape(nal)),
                    Err(crate::mvc::Bad::Unsupported(why)) if why == syntax::NOT_MULTIVIEW
                )
        })
}

/// Whether an `hvcC` decoder configuration record holds a NAL unit of a
/// layer above the base (`nuh_layer_id` > 0), as a record merged with an
/// `lhvC` does.
pub(crate) fn hvcc_has_layers(record: &[u8]) -> bool {
    if record.len() < 23 || record[0] != 1 {
        return false;
    }
    let mut rest = &record[23..];
    for _ in 0..record[22] {
        let [_, hi, lo, ref tail @ ..] = *rest else {
            return false;
        };
        rest = tail;
        for _ in 0..u16::from_be_bytes([hi, lo]) {
            let [hi, lo, ref tail @ ..] = *rest else {
                return false;
            };
            let len = usize::from(u16::from_be_bytes([hi, lo]));
            let Some(nal) = tail.get(..len) else {
                return false;
            };
            if layer_id(nal) > 0 {
                return true;
            }
            rest = &tail[len..];
        }
    }
    false
}

/// Pixels a second (both eyes together) beyond which a multiview stream
/// plays its base view in hardware rather than both in software: 4K at 60
/// frames a second, about what FFmpeg's HEVC decoder keeps up with on the
/// Frame's CPU alongside the compositor. A spatial video from an iPhone (two
/// 1080p eyes at 30) or a Vision Pro (two 2200x2200 at 30) is well within
/// it; Apple Immersive Video (two 4320x4320 at 90) is seven times more.
const SOFTWARE_BUDGET: f64 = 3840.0 * 2160.0 * 60.0;

/// Pixels a second of the rewritten stream (three pictures an access unit:
/// both views and a copy of the base) beyond which a multiview stream
/// doesn't go to the hardware decoder: 4K at 120 frames a second, within
/// what the Frame's decoder does. Apple Immersive Video (two 4320x4320 at
/// 90) is five times more.
const HARDWARE_BUDGET: f64 = 3840.0 * 2160.0 * 120.0;

/// How a multiview stream plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Both views rewritten for the hardware decoder.
    Hardware,
    /// Both views in software.
    Software,
    /// Too large for both: the base view in 2D.
    BaseAlone,
}

/// How stream `index` of `input` plays, `hardware` when the hardware
/// decoder is there to take it.
pub(crate) fn plan(input: &Input, index: usize, hardware: bool) -> Plan {
    let Some(&st) = input.streams().get(index) else {
        return Plan::Software;
    };
    // SAFETY: the stream and its parameters live as long as the input.
    let par = unsafe { &*(*st).codecpar };
    let fps = frame_rate(input, index);
    let eye = f64::from(par.width) * f64::from(par.height) * fps;
    if hardware && 3.0 * eye <= HARDWARE_BUDGET {
        match merged::configuration(par, fps) {
            // The hardware decoder gives pictures of more than 8 bits packed
            // (Q10C), which can't be put side by side.
            Ok(r) if r.bit_depth() > 8 => {
                log::info!("MV-HEVC in software: {} bits a sample", r.bit_depth());
            }
            Ok(_) => return Plan::Hardware,
            Err(why) => log::info!("MV-HEVC in software: {why}"),
        }
    }
    if fits_software(input, index) {
        Plan::Software
    } else {
        Plan::BaseAlone
    }
}

/// Stream `index`'s frame rate, 30 when it doesn't say.
pub(crate) fn frame_rate(input: &Input, index: usize) -> f64 {
    let Some(&st) = input.streams().get(index) else {
        return 30.0;
    };
    // SAFETY: the stream lives as long as the input.
    let st = unsafe { &*st };
    [st.avg_frame_rate, st.r_frame_rate]
        .map(crate::q2d)
        .into_iter()
        .find(|r| (1.0..=480.0).contains(r))
        .unwrap_or(30.0)
}

/// Whether the CPU decodes both views of stream `index` in time (see
/// [`SOFTWARE_BUDGET`]).
pub(crate) fn fits_software(input: &Input, index: usize) -> bool {
    let Some(&st) = input.streams().get(index) else {
        return true;
    };
    // SAFETY: the stream and its parameters live as long as the input.
    let par = unsafe { &*(*st).codecpar };
    let eye = f64::from(par.width) * f64::from(par.height) * frame_rate(input, index);
    2.0 * eye <= SOFTWARE_BUDGET
}

/// Whether both views of stream `index` are more than the CPU decodes in
/// time (see [`SOFTWARE_BUDGET`]).
#[cfg(test)]
pub(crate) fn too_large_for_software(input: &Input, index: usize) -> bool {
    plan(input, index, false) == Plan::BaseAlone
}

/// An `hvcC` record without the NAL units of layers above the base, and
/// without the arrays that leaves empty. None when it isn't one.
pub(crate) fn base_layer_record(record: &[u8]) -> Option<Vec<u8>> {
    if record.len() < 23 || record[0] != 1 {
        return None;
    }
    let mut out = record[..23].to_vec();
    let mut arrays = 0u8;
    let mut rest = &record[23..];
    for _ in 0..record[22] {
        let [kind, hi, lo, ref tail @ ..] = *rest else {
            return None;
        };
        rest = tail;
        let mut kept = Vec::new();
        let mut count = 0u16;
        for _ in 0..u16::from_be_bytes([hi, lo]) {
            let [hi, lo, ref tail @ ..] = *rest else {
                return None;
            };
            let len = usize::from(u16::from_be_bytes([hi, lo]));
            let nal = tail.get(..len)?;
            rest = &tail[len..];
            if layer_id(nal) == 0 {
                kept.extend_from_slice(&[hi, lo]);
                kept.extend_from_slice(nal);
                count += 1;
            }
        }
        if count > 0 {
            out.push(kind);
            out.extend_from_slice(&count.to_be_bytes());
            out.extend_from_slice(&kept);
            arrays += 1;
        }
    }
    out[22] = arrays;
    Some(out)
}

/// A NAL unit's `nuh_layer_id` (0 when cut too short to say).
fn layer_id(nal: &[u8]) -> u8 {
    match *nal {
        [b0, b1, ..] => ((b0 & 1) << 5) | (b1 >> 3),
        _ => 0,
    }
}

/// A packet's NAL units (each after a `length_size`-byte length) without
/// those of layers above the base; None when it has none of those, or is
/// cut short.
pub(crate) fn strip_upper_layers(data: &[u8], length_size: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    let mut rest = data;
    let mut stripped = false;
    while !rest.is_empty() {
        let head = rest.get(..length_size)?;
        let len = head.iter().fold(0usize, |n, &b| (n << 8) | usize::from(b));
        let unit = rest.get(..length_size + len)?;
        if layer_id(&unit[length_size..]) == 0 {
            out.extend_from_slice(unit);
        } else {
            stripped = true;
        }
        rest = &rest[length_size + len..];
    }
    stripped.then_some(out)
}

/// Whether the container says the views are reversed: the right eye's is
/// the base layer.
pub(crate) fn views_reversed(input: &Input, index: usize) -> bool {
    let Some(&st) = input.streams().get(index) else {
        return false;
    };
    // SAFETY: the stream's parameters live as long as the input.
    let par = unsafe { &*(*st).codecpar };
    crate::info::stereo3d_side_data(par)
        .is_some_and(|s| s.flags & ff::AV_STEREO3D_FLAG_INVERT as i32 != 0)
}

/// Which views a [`MvHevcDecoder`] gives out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Views {
    /// Both, side by side.
    Both,
    /// The base view in both halves of a picture twice as wide, as `Both`
    /// gives them (pictures of the video).
    BaseTwice,
    /// The base view as it is: a stream played in 2D.
    Base,
}

/// Which eye a decoded view is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Eye {
    Left,
    Right,
    /// A picture of a stream with one view: both eyes.
    Both,
}

/// A view waiting for the other eye's, with its time.
struct Waiting {
    frame: Frame,
    pts: i64,
}

/// Decodes multiview HEVC with `inner`, and pairs the views.
pub(crate) struct MvHevcDecoder {
    pub(crate) inner: Decoder,
    pub(crate) views: Views,
    /// The NAL length size of packets whose upper layers are taken out
    /// before `inner` gets them (a hardware decoder of the base view).
    strip: Option<usize>,
    /// The base view is the right eye's, unless the frames say otherwise.
    reversed: bool,
    /// The view id of the first view decoded: the base layer's.
    base_view: Option<i32>,
    /// One eye's view of the access unit being decoded.
    first: Option<(Eye, Waiting)>,
    scratch: Frame,
}

impl MvHevcDecoder {
    /// `strip`: as the field.
    pub(crate) fn new(
        inner: Decoder,
        views: Views,
        strip: Option<usize>,
        reversed: bool,
    ) -> MvHevcDecoder {
        MvHevcDecoder {
            inner,
            views,
            strip,
            reversed,
            base_view: None,
            first: None,
            scratch: Frame::new(),
        }
    }

    /// Which eye the frame in `scratch` is.
    fn eye(&mut self) -> Eye {
        // SAFETY: a decoded frame; side data is checked for size before use.
        unsafe {
            let f = self.scratch.as_ptr();
            let sd = ff::av_frame_get_side_data(f, ff::AV_FRAME_DATA_STEREO3D);
            if !sd.is_null() && (*sd).size >= size_of::<ff::AVStereo3D>() {
                match (*((*sd).data as *const ff::AVStereo3D)).view {
                    ff::AV_STEREO3D_VIEW_LEFT => return Eye::Left,
                    ff::AV_STEREO3D_VIEW_RIGHT => return Eye::Right,
                    _ => {}
                }
            }
            let sd = ff::av_frame_get_side_data(f, ff::AV_FRAME_DATA_VIEW_ID);
            if sd.is_null() || (*sd).size < size_of::<i32>() {
                return Eye::Both;
            }
            let view = *((*sd).data as *const i32);
            let base = *self.base_view.get_or_insert(view);
            match (view == base, self.reversed) {
                (true, false) | (false, true) => Eye::Left,
                _ => Eye::Right,
            }
        }
    }

    /// Sends a packet, as [`Decoder::send`].
    pub(crate) fn send(&mut self, pkt: *const ff::AVPacket) -> Result<bool> {
        let Some(length_size) = self.strip.filter(|_| !pkt.is_null()) else {
            return self.inner.send(pkt);
        };
        // SAFETY: a valid packet holding `size` bytes at `data`.
        let data = unsafe {
            let p = &*pkt;
            if p.data.is_null() {
                return self.inner.send(pkt);
            }
            std::slice::from_raw_parts(p.data, p.size.max(0) as usize)
        };
        let Some(base) = strip_upper_layers(data, length_size) else {
            return self.inner.send(pkt);
        };
        let copy = Packet::new();
        if copy.as_ptr().is_null() {
            return Err(Error::Unsupported("out of memory for a packet".into()));
        }
        // SAFETY: a fresh packet, given a buffer of base.len() bytes that is
        // then filled from base, and the caller's packet's properties.
        unsafe {
            check(
                ff::av_new_packet(copy.as_ptr(), base.len() as i32),
                "base view packet",
            )?;
            std::ptr::copy_nonoverlapping(base.as_ptr(), (*copy.as_ptr()).data, base.len());
            check(
                ff::av_packet_copy_props(copy.as_ptr(), pkt),
                "base view packet",
            )?;
        }
        self.inner.send(copy.as_ptr())
    }

    /// Receives a side-by-side frame, as [`Decoder::receive`].
    pub(crate) fn receive(&mut self, frame: &mut Frame) -> Result<Option<bool>> {
        if self.views == Views::Base {
            let r = self.inner.receive(frame)?;
            if r == Some(true) {
                // SAFETY: a decoded frame.
                unsafe { drop_view_labels(frame.as_ptr()) };
            }
            return Ok(r);
        }
        loop {
            match self.inner.receive(&mut self.scratch)? {
                None => return Ok(None),
                Some(false) => {
                    // The end: a view still waiting goes out alone.
                    if let Some((_, lone)) = self.first.take() {
                        pair(frame, &lone, &lone.frame, &lone.frame)?;
                        return Ok(Some(true));
                    }
                    return Ok(Some(false));
                }
                Some(true) => {}
            }
            let eye = if self.views == Views::Both {
                self.eye()
            } else {
                Eye::Both
            };
            // SAFETY: a decoded frame.
            let pts = unsafe { (*self.scratch.as_ptr()).best_effort_timestamp };
            let decoded = Waiting {
                frame: Frame(self.scratch.take_raw()),
                pts,
            };
            match (eye, self.first.take()) {
                (Eye::Both, lone) => {
                    // A view whose other eye never came is dropped: the
                    // picture after it is whole.
                    drop(lone);
                    pair(frame, &decoded, &decoded.frame, &decoded.frame)?;
                    return Ok(Some(true));
                }
                (eye, Some((other, waiting))) if other != eye && waiting.pts == pts => {
                    let (left, right) = match eye {
                        Eye::Right => (&waiting.frame, &decoded.frame),
                        _ => (&decoded.frame, &waiting.frame),
                    };
                    pair(frame, &waiting, left, right)?;
                    return Ok(Some(true));
                }
                (eye, Some((_, lone))) => {
                    // A view whose other eye never came goes out alone,
                    // and this one waits for its own.
                    self.first = Some((eye, decoded));
                    pair(frame, &lone, &lone.frame, &lone.frame)?;
                    return Ok(Some(true));
                }
                (eye, None) => self.first = Some((eye, decoded)),
            }
        }
    }

    pub(crate) fn flush(&mut self) {
        self.first = None;
        self.inner.flush();
    }
}

/// Puts `left` and `right` side by side into `out`, with `time`'s timing,
/// without the per-view labels the decoder gave them: `out` holds both.
fn pair(out: &mut Frame, time: &Waiting, left: &Frame, right: &Frame) -> Result<()> {
    // SAFETY: both are decoded frames of the same stream; out is ours to fill.
    unsafe {
        side_by_side(out.as_ptr(), left.as_ptr(), right.as_ptr())?;
        let o = out.as_ptr();
        drop_view_labels(o);
        (*o).pts = time.pts;
        (*o).best_effort_timestamp = time.pts;
    }
    Ok(())
}

/// Takes off the labels the decoder gives each view's frame.
///
/// # Safety
/// `frame` is a valid frame.
unsafe fn drop_view_labels(frame: *mut ff::AVFrame) {
    // SAFETY: per the contract.
    unsafe {
        ff::av_frame_remove_side_data(frame, ff::AV_FRAME_DATA_STEREO3D);
        ff::av_frame_remove_side_data(frame, ff::AV_FRAME_DATA_VIEW_ID);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::HwDecode;
    use crate::info::read_info;
    use crate::info::tests::open;
    use crate::input::Input;
    use fp_core::format::StereoLayout;

    /// An `hvcC` with one array holding `nals`.
    fn hvcc(nals: &[&[u8]]) -> Vec<u8> {
        let mut c = vec![1u8];
        c.extend_from_slice(&[0; 21]);
        c.push(1); // one array
        c.push(33); // SPS
        c.extend_from_slice(&(nals.len() as u16).to_be_bytes());
        for n in nals {
            c.extend_from_slice(&(n.len() as u16).to_be_bytes());
            c.extend_from_slice(n);
        }
        c
    }

    #[test]
    fn a_configuration_with_a_second_layer_is_multiview() {
        // nal_unit_type 33, nuh_layer_id 0, then 1.
        let base: &[u8] = &[33 << 1, 1, 0xff];
        let layer1: &[u8] = &[33 << 1, (1 << 3) | 1, 0xff];
        assert!(!hvcc_has_layers(&hvcc(&[base])));
        assert!(hvcc_has_layers(&hvcc(&[base, layer1])));
        // Cut short: not read past the end.
        let mut short = hvcc(&[base, layer1]);
        short.truncate(short.len() - 2);
        assert!(!hvcc_has_layers(&short));
        assert!(!hvcc_has_layers(&[]));
    }

    #[test]
    fn the_base_layer_record_keeps_the_base_layer_alone() {
        let base: &[u8] = &[33 << 1, 1, 0xff];
        let layer1: &[u8] = &[33 << 1, (1 << 3) | 1, 0xee];
        assert_eq!(
            base_layer_record(&hvcc(&[base, layer1])).unwrap(),
            hvcc(&[base])
        );
        // An array left empty goes.
        let mut only_upper = hvcc(&[layer1]);
        let stripped = base_layer_record(&only_upper).unwrap();
        assert_eq!(stripped.len(), 23);
        assert_eq!(stripped[22], 0);
        only_upper.truncate(only_upper.len() - 1);
        assert!(base_layer_record(&only_upper).is_none());
    }

    #[test]
    fn packets_lose_the_upper_layers_units() {
        let unit = |nal: &[u8]| {
            let mut u = (nal.len() as u32).to_be_bytes().to_vec();
            u.extend_from_slice(nal);
            u
        };
        let base = unit(&[1 << 1, 1, 0xaa, 0xbb]);
        let upper = unit(&[1 << 1, (1 << 3) | 1, 0xcc]);
        let both = [base.clone(), upper.clone(), base.clone()].concat();
        assert_eq!(
            strip_upper_layers(&both, 4).unwrap(),
            [base.clone(), base.clone()].concat()
        );
        assert!(
            strip_upper_layers(&base, 4).is_none(),
            "nothing to take out"
        );
        assert!(strip_upper_layers(&both[..both.len() - 1], 4).is_none());
    }

    const FIXTURE: &str = "mv_hevc/multiview.mov";

    #[test]
    fn a_spatial_video_is_found_and_decodes_twice_as_wide() {
        let input = open(FIXTURE);
        let info = read_info(&input);
        let v = info.video_stream().unwrap();
        assert!(v.mv_hevc);
        assert!(!v.mvc && !v.frame_sequential);
        assert_eq!(info.hints.stereo, Some(StereoLayout::SideBySide));
        assert!(!info.hints.eyes_swapped);
        // SAFETY: the stream belongs to the open input.
        let eye = unsafe { (*(*input.streams()[v.index]).codecpar).width } as u32;
        assert_eq!(v.width, eye * 2);
    }

    #[test]
    fn a_plain_hevc_file_is_not_multiview() {
        let input = open("hevc10_tb.mkv");
        let v = read_info(&input).video_stream().unwrap().index;
        assert!(!is_multiview(&input, v));
    }

    /// Decodes `name`, giving each picture's time, width and whether its
    /// halves differ.
    fn decode(name: &str, hw: HwDecode) -> (String, Vec<(f64, i32, bool)>) {
        let mut input = open(name);
        let vi = read_info(&input).video_stream().unwrap().index;
        let mut dec = Decoder::open(&input, vi, hw).unwrap();
        let (mut pkt, mut frame) = (Packet::new(), Frame::new());
        let mut out = Vec::new();
        let mut ended = false;
        loop {
            match dec.receive(&mut frame).unwrap() {
                Some(true) => {
                    // SAFETY: a decoded frame.
                    let width = unsafe { (*frame.as_ptr()).width };
                    let time = dec.frame_time(&frame).unwrap_or(-1.0);
                    out.push((time, width, halves_differ(&frame)));
                    if hw == HwDecode::Stills {
                        return (dec.name.clone(), out);
                    }
                    continue;
                }
                Some(false) => return (dec.name.clone(), out),
                None => {}
            }
            if !ended && input.read(pkt.as_ptr()).unwrap() {
                if pkt.stream_index() == vi {
                    dec.send(pkt.as_ptr()).unwrap();
                }
                pkt.unref();
            } else if !ended {
                ended = true;
                dec.send(std::ptr::null()).unwrap();
            }
        }
    }

    /// Whether the left and right halves of the frame's luma differ.
    fn halves_differ(frame: &Frame) -> bool {
        // SAFETY: a decoded frame, its first plane holding height rows.
        unsafe {
            let f = &*frame.as_ptr();
            let (w, h) = (f.width as usize, f.height as usize);
            (0..h).any(|y| {
                let row = std::slice::from_raw_parts(
                    f.data[0].offset(y as isize * f.linesize[0] as isize),
                    w,
                );
                row[..w / 2] != row[w / 2..]
            })
        }
    }

    #[test]
    fn both_views_of_each_picture_go_side_by_side() {
        let input = open(FIXTURE);
        let info = read_info(&input);
        let v = info.video_stream().unwrap();
        let (name, pictures) = decode(FIXTURE, HwDecode::Off);
        assert!(name.contains("MV-HEVC 3D"), "{name}");
        assert!(!pictures.is_empty());
        let mut last = f64::NEG_INFINITY;
        for &(time, width, differ) in &pictures {
            assert_eq!(width as u32, v.width);
            assert!(differ, "both eyes the same at {time}");
            assert!(time > last, "{time} after {last}");
            last = time;
        }
        // One picture per frame of the film, not one per view.
        let expected = (info.duration * v.fps).round() as usize;
        assert!(
            pictures.len().abs_diff(expected) <= 1,
            "{} {expected}",
            pictures.len()
        );
    }

    /// The first picture `dec` gives for `input`'s stream `vi`.
    fn first_picture(dec: &mut Decoder, input: &mut Input, vi: usize) -> Frame {
        let (mut pkt, mut frame) = (Packet::new(), Frame::new());
        let mut ended = false;
        loop {
            match dec.receive(&mut frame).unwrap() {
                Some(true) => return frame,
                Some(false) => panic!("no picture"),
                None => {}
            }
            // Frame threads hold pictures back until the end of a clip
            // this short.
            if input.read(pkt.as_ptr()).unwrap() {
                if pkt.stream_index() == vi {
                    dec.send(pkt.as_ptr()).unwrap();
                }
                pkt.unref();
            } else if !ended {
                ended = true;
                dec.send(std::ptr::null()).unwrap();
            }
        }
    }

    /// The fixture's SEI says its base view (view 0) is the right eye's, and
    /// the second (view 1, almost black) the left eye's.
    #[test]
    fn each_view_goes_to_the_eye_the_stream_names() {
        let mut input = open(FIXTURE);
        let vi = read_info(&input).video_stream().unwrap().index;
        let mut dec = Decoder::open(&input, vi, HwDecode::Off).unwrap();
        let both = first_picture(&mut dec, &mut input, vi);

        let mut input = open(FIXTURE);
        let st = input.streams()[vi];
        // SAFETY: the stream belongs to the open input.
        let (par, time_base) = unsafe { ((*st).codecpar, (*st).time_base) };
        let mut plain = Decoder::open_with(par, time_base, HwDecode::Off, false).unwrap();
        let base = first_picture(&mut plain, &mut input, vi);
        // SAFETY: decoded frames, their first planes holding height rows.
        unsafe {
            let (b, s) = (&*base.as_ptr(), &*both.as_ptr());
            assert_eq!(s.width, b.width * 2);
            for y in 0..b.height as isize {
                let row = |f: &ff::AVFrame, x: usize, w: usize| {
                    std::slice::from_raw_parts(
                        f.data[0].offset(y * f.linesize[0] as isize).add(x),
                        w,
                    )
                };
                let w = b.width as usize;
                assert_eq!(row(b, 0, w), row(s, w, w), "row {y}");
                assert!(row(s, 0, w).iter().all(|&l| l < 32), "row {y}");
            }
        }
    }

    #[test]
    fn a_stream_too_large_for_software_plays_its_base_view_alone() {
        let mut input = open(FIXTURE);
        let vi = read_info(&input).video_stream().unwrap().index;
        assert!(!too_large_for_software(&input, vi));
        let st = input.streams()[vi];
        // SAFETY: the stream belongs to the open input.
        let (par, time_base) = unsafe { ((*st).codecpar, (*st).time_base) };
        let mode = crate::decode::MvHevcMode::BaseAlone(HwDecode::Auto);
        let mut dec = Decoder::open_mv_hevc(par, time_base, mode, false).unwrap();
        assert!(dec.name.contains("base view"), "{}", dec.name);
        let frame = first_picture(&mut dec, &mut input, vi);
        // SAFETY: the stream belongs to the open input; a decoded frame.
        unsafe {
            assert_eq!((*frame.as_ptr()).width, (*par).width);
            let views = ff::av_frame_get_side_data(frame.as_ptr(), ff::AV_FRAME_DATA_VIEW_ID);
            assert!(views.is_null());
        }
    }

    /// Decoded pictures: each one's time, rows plane by plane, and width.
    type Pictures = Vec<(i64, Vec<Vec<u8>>, usize)>;

    /// A decoded 4:2:0 frame's time, rows plane by plane, and width.
    fn rows(frame: &Frame) -> (i64, Vec<Vec<u8>>, usize) {
        // SAFETY: a decoded frame: its planes hold its rows.
        unsafe {
            let f = &*frame.as_ptr();
            let mut planes = Vec::new();
            for p in 0..3 {
                let h = if p == 0 { f.height } else { f.height / 2 };
                let w = ff::av_image_get_linesize(f.format, f.width, p as i32) as usize;
                let mut bytes = Vec::new();
                for y in 0..h as isize {
                    bytes.extend_from_slice(std::slice::from_raw_parts(
                        f.data[p].offset(y * f.linesize[p] as isize),
                        w,
                    ));
                }
                planes.push(bytes);
            }
            (f.pts, planes, f.width as usize)
        }
    }

    /// A decoder opened to rewrite the stream did, to the end: it never
    /// gave up and went on in software, which would give the very same
    /// pictures.
    fn assert_rewritten(dec: &mut Decoder, mode: crate::decode::MvHevcMode) {
        if matches!(mode, crate::decode::MvHevcMode::Merged(_)) {
            let gave_up = dec.merged().and_then(|m| m.gave_up.clone());
            assert_eq!(gave_up, None, "the rewrite gave up");
        }
    }

    /// Every side-by-side picture of `name`, by `mode`.
    fn pictures(name: &str, mode: crate::decode::MvHevcMode) -> Pictures {
        let mut input = open(name);
        let vi = read_info(&input).video_stream().unwrap().index;
        let st = input.streams()[vi];
        // SAFETY: the stream belongs to the open input.
        let (par, time_base) = unsafe { ((*st).codecpar, (*st).time_base) };
        let mut dec = Decoder::open_mv_hevc(par, time_base, mode, false).unwrap();
        let (mut pkt, mut frame) = (Packet::new(), Frame::new());
        let mut out = Vec::new();
        let mut ended = false;
        loop {
            match dec.receive(&mut frame).unwrap() {
                Some(true) => {
                    out.push(rows(&frame));
                    continue;
                }
                Some(false) => {
                    assert_rewritten(&mut dec, mode);
                    return out;
                }
                None => {}
            }
            if input.read(pkt.as_ptr()).unwrap() {
                if pkt.stream_index() == vi {
                    dec.send(pkt.as_ptr()).unwrap();
                }
                pkt.unref();
            } else if !ended {
                ended = true;
                dec.send(std::ptr::null()).unwrap();
            }
        }
    }

    /// Whether `name`, rewritten and decoded by a plain HEVC decoder, gives
    /// the very pictures FFmpeg's MV-HEVC decoder does; how many.
    fn rewrite_matches(name: &str) -> usize {
        use crate::decode::MvHevcMode;
        let software = pictures(name, MvHevcMode::BothViews);
        let merged = pictures(name, MvHevcMode::Merged(HwDecode::Off));
        assert_eq!(merged.len(), software.len());
        for (i, (a, b)) in software.iter().zip(&merged).enumerate() {
            assert_eq!(a.0, b.0, "picture {i}'s time");
            for p in 0..3 {
                let differ = a.1[p].iter().zip(&b.1[p]).filter(|(x, y)| x != y).count();
                assert!(
                    differ == 0,
                    "picture {i} plane {p}: {differ} samples differ"
                );
            }
        }
        software.len()
    }

    #[test]
    fn the_rewritten_stream_decodes_into_both_views_exactly() {
        assert_eq!(rewrite_matches(FIXTURE), 11);
    }

    /// The fixture's pictures through the rewrite, `hook` called on the
    /// decoder after its `at`th packet: what it gave before, and after
    /// (when `hook` says to start the input again, from the start).
    fn rewritten_around(
        at: usize,
        hook: impl FnOnce(&mut Decoder) -> bool,
    ) -> (Pictures, Pictures) {
        let mut input = open(FIXTURE);
        let vi = read_info(&input).video_stream().unwrap().index;
        let st = input.streams()[vi];
        // SAFETY: the stream belongs to the open input.
        let (par, time_base) = unsafe { ((*st).codecpar, (*st).time_base) };
        let mode = crate::decode::MvHevcMode::Merged(HwDecode::Off);
        let mut dec = Decoder::open_mv_hevc(par, time_base, mode, false).unwrap();
        let (mut pkt, mut frame) = (Packet::new(), Frame::new());
        let (mut before, mut after) = (Vec::new(), Vec::new());
        let mut hook = Some(hook);
        let (mut sent, mut ended, mut hooked) = (0, false, false);
        loop {
            match dec.receive(&mut frame).unwrap() {
                Some(true) => {
                    if hooked { &mut after } else { &mut before }.push(rows(&frame));
                    continue;
                }
                Some(false) => return (before, after),
                None => {}
            }
            if sent == at && !hooked {
                hooked = true;
                if hook.take().unwrap()(&mut dec) {
                    input = open(FIXTURE);
                }
            }
            if input.read(pkt.as_ptr()).unwrap() {
                if pkt.stream_index() == vi {
                    dec.send(pkt.as_ptr()).unwrap();
                    sent += 1;
                }
                pkt.unref();
            } else if !ended {
                ended = true;
                dec.send(std::ptr::null()).unwrap();
            }
        }
    }

    /// After a flush (a seek back to the start), the rewrite starts again
    /// and gives every picture as it first did.
    #[test]
    fn the_rewrite_starts_again_after_a_flush() {
        let software = pictures(FIXTURE, crate::decode::MvHevcMode::BothViews);
        let (_, after) = rewritten_around(5, |dec| {
            dec.flush();
            true
        });
        assert!(
            after == software,
            "{} pictures after the flush",
            after.len()
        );
    }

    /// Meeting what the rewrite can't express halfway, the stream goes on
    /// in software: every picture once, as the software decoder gives it.
    #[test]
    fn a_rewrite_given_up_goes_on_in_software() {
        let software = pictures(FIXTURE, crate::decode::MvHevcMode::BothViews);
        let (before, after) = rewritten_around(6, |dec| {
            dec.merged().unwrap().give_up_now();
            false
        });
        let all: Vec<_> = before.into_iter().chain(after).collect();
        assert!(
            all == software,
            "{} pictures, {} in software",
            all.len(),
            software.len()
        );
    }

    /// Given up after the end of the stream was sent, the decoder playing
    /// on is told of the end too: every picture once, then the end.
    #[test]
    fn a_rewrite_given_up_at_the_end_still_ends() {
        let software = pictures(FIXTURE, crate::decode::MvHevcMode::BothViews);
        let mut input = open(FIXTURE);
        let vi = read_info(&input).video_stream().unwrap().index;
        let st = input.streams()[vi];
        // SAFETY: the stream belongs to the open input.
        let (par, time_base) = unsafe { ((*st).codecpar, (*st).time_base) };
        let mode = crate::decode::MvHevcMode::Merged(HwDecode::Off);
        let mut dec = Decoder::open_mv_hevc(par, time_base, mode, false).unwrap();
        let (mut pkt, mut frame) = (Packet::new(), Frame::new());
        let mut got = Vec::new();
        let mut send = |dec: &mut Decoder, p: *const ff::AVPacket, got: &mut Pictures| {
            while !dec.send(p).unwrap() {
                if dec.receive(&mut frame).unwrap() == Some(true) {
                    got.push(rows(&frame));
                }
            }
        };
        while input.read(pkt.as_ptr()).unwrap() {
            if pkt.stream_index() == vi {
                send(&mut dec, pkt.as_ptr(), &mut got);
            }
            pkt.unref();
        }
        send(&mut dec, std::ptr::null(), &mut got);
        dec.merged().unwrap().give_up_now();
        let mut frame = Frame::new();
        for _ in 0..1000 {
            match dec.receive(&mut frame).unwrap() {
                Some(true) => got.push(rows(&frame)),
                Some(false) => {
                    assert!(got == software, "{} pictures", got.len());
                    return;
                }
                None => {}
            }
        }
        panic!("the stream never ended");
    }

    /// An Annex B stream's parameter sets (the first access unit's) and its
    /// access units.
    fn access_units(data: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut units: Vec<Vec<u8>> = Vec::new();
        let mut current = Vec::new();
        let mut has_vcl = false;
        let mut sets = Vec::new();
        for nal in crate::nal::units(data, 0) {
            let h = syntax::NalHeader::parse(nal).unwrap();
            let starts = if h.kind < 32 {
                h.layer == 0 && nal[2] & 0x80 != 0
            } else {
                h.layer == 0 && matches!(h.kind, 32..=35 | 39)
            };
            if starts && has_vcl {
                units.push(std::mem::take(&mut current));
                has_vcl = false;
            }
            has_vcl |= h.kind < 32;
            if units.is_empty() && matches!(h.kind, 32..=34) {
                sets.extend_from_slice(&[0, 0, 0, 1]);
                sets.extend_from_slice(nal);
            }
            current.extend_from_slice(&[0, 0, 0, 1]);
            current.extend_from_slice(nal);
        }
        units.push(current);
        (sets, units)
    }

    /// The side-by-side pictures `mode` decodes from Annex B access units.
    /// The pictures of `units` by `mode`, and why its rewrite gave up, if it
    /// did.
    fn decode_units(
        sets: &[u8],
        units: &[Vec<u8>],
        mode: crate::decode::MvHevcMode,
    ) -> (Pictures, Option<String>) {
        // SAFETY: fresh parameters of our own, given a copy of the sets.
        let mut par = unsafe {
            let p = ff::avcodec_parameters_alloc();
            (*p).codec_type = ff::AVMEDIA_TYPE_VIDEO;
            (*p).codec_id = ff::AV_CODEC_ID_HEVC;
            let buf = ff::av_mallocz(sets.len() + ff::AV_INPUT_BUFFER_PADDING_SIZE as usize);
            std::ptr::copy_nonoverlapping(sets.as_ptr(), buf.cast(), sets.len());
            (*p).extradata = buf.cast();
            (*p).extradata_size = sets.len() as i32;
            p
        };
        let tb = ff::AVRational { num: 1, den: 30 };
        let mut dec = Decoder::open_mv_hevc(par, tb, mode, false).unwrap();
        let mut out = Vec::new();
        let mut frame = Frame::new();
        let mut take = |dec: &mut Decoder, out: &mut Vec<_>| loop {
            match dec.receive(&mut frame).unwrap() {
                Some(true) => out.push(rows(&frame)),
                Some(false) => return true,
                None => return false,
            }
        };
        for (i, u) in units.iter().enumerate() {
            let pkt = Packet::new();
            // SAFETY: a fresh packet given a buffer of u's size, filled.
            unsafe {
                let p = pkt.as_ptr();
                assert!(ff::av_new_packet(p, u.len() as i32) >= 0);
                std::ptr::copy_nonoverlapping(u.as_ptr(), (*p).data, u.len());
                (*p).pts = i as i64;
                (*p).dts = i as i64;
            }
            while !dec.send(pkt.as_ptr()).unwrap() {
                take(&mut dec, &mut out);
            }
            take(&mut dec, &mut out);
        }
        dec.send(std::ptr::null()).unwrap();
        while !take(&mut dec, &mut out) {}
        let gave_up = dec.merged().and_then(|m| m.gave_up.clone());
        // SAFETY: ours.
        unsafe { ff::avcodec_parameters_free(&mut par) };
        (out, gave_up)
    }

    /// The MV-HEVC conformance streams FFmpeg's tests decode (from its FATE
    /// suite, `hevc-conformance/`), in the folder `FPX_MV_HEVC_SAMPLES`
    /// names, rewritten exactly. MVHEVCS_B predicts motion from the other
    /// view, which the rewrite can't express: it gives up there, and the
    /// pictures are the same through that too.
    #[test]
    #[ignore = "needs the conformance streams"]
    fn conformance_streams_rewrite_exactly() {
        use crate::decode::MvHevcMode;
        let dir = std::env::var("FPX_MV_HEVC_SAMPLES").unwrap();
        let streams = [
            ("MVHEVCS_A.bit", None),
            (
                "MVHEVCS_B.bit",
                Some("motion predicted from the other view"),
            ),
            ("mv_nuh_layer_id.bit", None),
        ];
        for (name, gives_up) in streams {
            let data = std::fs::read(format!("{dir}/{name}")).unwrap();
            let (sets, units) = access_units(&data);
            let (software, _) = decode_units(&sets, &units, MvHevcMode::BothViews);
            let (merged, gave_up) = decode_units(&sets, &units, MvHevcMode::Merged(HwDecode::Off));
            assert_eq!(gave_up.as_deref(), gives_up, "{name}");
            // Their times are the decoding order's, which FFmpeg's guess
            // replaces for some: only the pictures are compared.
            assert_eq!(merged.len(), software.len(), "{name}");
            for (i, (a, b)) in software.iter().zip(&merged).enumerate() {
                assert!(a.1 == b.1, "{name}: picture {i} differs");
            }
            println!("{name}: {} pictures the same", merged.len());
        }
    }

    #[test]
    #[ignore = "prints the fixture's structure"]
    fn dump_fixture() {
        use crate::mvc::bits::unescape;
        use syntax::*;
        let mut input = open(FIXTURE);
        let vi = read_info(&input).video_stream().unwrap().index;
        let st = input.streams()[vi];
        // SAFETY: the stream belongs to the open input.
        let par = unsafe { &*(*st).codecpar };
        let extradata = crate::mvc::extradata(par).to_vec();
        let length_size = usize::from(extradata[21] & 3) + 1;
        let mut vps_ = None;
        let mut spss = std::collections::HashMap::new();
        let mut ppss = std::collections::HashMap::new();
        let show = |nal: &[u8],
                    vps_: &mut Option<Vps>,
                    spss: &mut std::collections::HashMap<u32, Sps>,
                    ppss: &mut std::collections::HashMap<u32, Pps>| {
            let h = NalHeader::parse(nal).unwrap();
            let rbsp = unescape(nal);
            match h.kind {
                VPS => {
                    let v = vps(&rbsp);
                    println!("VPS {h:?} {v:?}");
                    *vps_ = v.ok();
                }
                SPS => {
                    let s = sps(&rbsp, h.layer, vps_.as_ref());
                    println!("SPS {h:?} {s:?}");
                    if let Ok(s) = s {
                        spss.insert(s.id, s);
                    }
                }
                PPS => {
                    let p = pps(&rbsp, |id| spss.get(&id));
                    println!("PPS {h:?} {p:?}");
                    if let Ok(p) = p {
                        ppss.insert(p.id, p);
                    }
                }
                39 | 40 => println!("SEI {h:?} eyes {:?}", eye_views(&rbsp)),
                k if k < 32 => {
                    let mut r = crate::mvc::bits::Reader::new(&rbsp, 16);
                    r.flag().unwrap();
                    if h.irap() {
                        r.flag().unwrap();
                    }
                    let pid = r.ue().unwrap();
                    let p = &ppss[&pid];
                    let sp = &spss[&p.sps_id];
                    let s = slice(&rbsp, h, p, sp, vps_.as_ref(), None);
                    match s {
                        Ok(s) => println!(
                            "  slice {h:?} pps {pid} type {} poc {:?} rps {:?} il {} tmvp {} refs {:?} lists {:?} col {:?}",
                            s.slice_type,
                            s.poc_lsb,
                            s.st_rps,
                            s.inter_layer,
                            s.temporal_mvp,
                            s.num_ref_idx,
                            s.list_entry,
                            s.collocated
                        ),
                        Err(e) => println!("  slice {h:?} {e:?}"),
                    }
                }
                _ => println!("NAL {h:?}"),
            }
        };
        if let Ok(path) = std::env::var("FPX_DUMP") {
            let data = std::fs::read(path).unwrap();
            for (n, u) in access_units(&data).1.iter().enumerate() {
                println!("unit {n}");
                for nal in crate::nal::units(u, 0) {
                    show(nal, &mut vps_, &mut spss, &mut ppss);
                }
            }
            return;
        }
        for nal in record_units(&extradata) {
            show(nal, &mut vps_, &mut spss, &mut ppss);
        }
        let mut pkt = Packet::new();
        let mut n = 0;
        while input.read(pkt.as_ptr()).unwrap() {
            if pkt.stream_index() == vi {
                // SAFETY: a read packet.
                let data = unsafe {
                    let p = &*pkt.as_ptr();
                    std::slice::from_raw_parts(p.data, p.size as usize)
                };
                // SAFETY: as above.
                let pts = unsafe { (*pkt.as_ptr()).pts };
                println!("packet {n} pts {pts}");
                for nal in crate::nal::units(data, length_size) {
                    show(nal, &mut vps_, &mut spss, &mut ppss);
                }
                n += 1;
            }
            pkt.unref();
        }
    }

    #[test]
    fn a_still_is_the_base_view_twice() {
        let (name, pictures) = decode(FIXTURE, HwDecode::Stills);
        assert!(name.contains("base view"), "{name}");
        let (_, width, differ) = pictures[0];
        assert_eq!(
            width as u32,
            read_info(&open(FIXTURE)).video_stream().unwrap().width
        );
        assert!(!differ);
    }
}
