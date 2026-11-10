//! Both views of multiview HEVC through an ordinary HEVC decoder (the
//! Frame's hardware one): each access unit rewritten ([`super::rewrite`])
//! into pictures of one stream, the views decoded paired side by side.
//!
//! When the rewrite meets what it can't express, the stream goes on in
//! software from the last keyframe, through FFmpeg's MV-HEVC decoder.

use super::rewrite::{Rewriter, Slot};
use crate::decode::{Decoder, Frame, HwDecode, MvHevcMode, Packet, Skip};
use crate::mvc::{Bad, side_by_side};
use crate::{Error, Result, check};
use fp_ffmpeg_sys as ff;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// What a picture of the rewritten stream is, by the tag it decodes under.
#[derive(Clone, Copy, Debug)]
struct Tag {
    pts: i64,
    duration: i64,
    au: u64,
    slot: Slot,
}

/// Base pictures waiting for their second view past this many go out
/// alone, the base view in both eyes.
const WAITING: usize = 8;

/// Packets since the last keyframe kept to replay in software.
const HISTORY: usize = 600;

/// Access units in a row the rewriter can't read before the stream goes on
/// without it.
const BROKEN: u32 = 3;

pub(crate) struct MergedDecoder {
    /// An HEVC decoder of the rewritten stream (Annex B, no extradata),
    /// until the rewrite gives up: closed then, so its hardware session is
    /// free for the decoder playing on.
    inner: Option<Decoder>,
    /// `inner`'s name as it was opened: under another, it has fallen back
    /// to another decoder, which starts without the stream's parameter
    /// sets.
    inner_name: String,
    rewriter: Rewriter,
    length_size: usize,
    /// The stream's parameters, to open the software decoder with.
    par: *mut ff::AVCodecParameters,
    time_base: ff::AVRational,
    reversed: bool,
    hw: HwDecode,
    /// Whether the CPU decodes both views in time: else, without the
    /// hardware decoder, the base view plays in both eyes.
    software_ok: bool,
    /// Cuts the decoders' waits short (see [`Decoder::set_interrupt`]).
    interrupt: Option<Arc<AtomicBool>>,
    /// Why the rewrite gave up, and the decoder playing on.
    pub(crate) gave_up: Option<String>,
    pub(crate) fallback: Option<Decoder>,
    /// After giving up, the pictures shown up to this pts aren't again.
    shown: Option<i64>,
    /// The last picture given out's pts.
    last_out: Option<i64>,
    /// Packets from the last keyframe on, unless `history_whole` is false:
    /// more than [`HISTORY`] since.
    history: Vec<Packet>,
    history_whole: bool,
    /// After giving up without the packets since the last keyframe, the
    /// decoder playing on takes none before the next.
    await_key: bool,
    /// History the decoder playing on hasn't taken yet.
    replay: VecDeque<Packet>,
    /// The end of the stream was sent; and, after giving up, is still to
    /// send on (once `replay` is).
    ended: bool,
    end_owed: bool,
    /// Access units in a row the rewriter couldn't read.
    broken: u32,
    queue: VecDeque<Packet>,
    tags: HashMap<i64, Tag>,
    next_tag: i64,
    aus: u64,
    /// Base pictures in the order they were decoded out, and second views
    /// waiting for theirs.
    bases: VecDeque<(Frame, Tag)>,
    seconds: HashMap<u64, Frame>,
    scratch: Frame,
    pub(crate) skip: Option<Skip>,
    /// The precise seek `skip` is, as [`Decoder::skip_before`] takes it.
    pub(crate) skip_seconds: Option<(f64, f64)>,
}

// SAFETY: one thread drives a decoder at a time; `par` is owned.
unsafe impl Send for MergedDecoder {}

impl Drop for MergedDecoder {
    fn drop(&mut self) {
        // SAFETY: owned (null-safe).
        unsafe { ff::avcodec_parameters_free(&mut self.par) };
    }
}

/// Whether the rewrite takes the configuration of a stream with these
/// parameters: its parameter sets read, both views coded alike.
pub(crate) fn configuration(
    par: &ff::AVCodecParameters,
    fps: f64,
) -> std::result::Result<Rewriter, String> {
    let mut rewriter = Rewriter::new(fps);
    for nal in configuration_units(crate::mvc::extradata(par)) {
        match rewriter.parameter_set(nal) {
            Ok(()) => {}
            Err(Bad::Unsupported(why)) => return Err(why),
            Err(Bad::Broken(why)) => return Err(why.into()),
        }
    }
    rewriter.check().map_err(|e| match e {
        Bad::Unsupported(why) => why,
        Bad::Broken(why) => why.into(),
    })?;
    Ok(rewriter)
}

/// The NAL units of a decoder configuration: an `hvcC` record's, or Annex
/// B's.
pub(super) fn configuration_units(extradata: &[u8]) -> Vec<&[u8]> {
    if extradata.first() == Some(&1) {
        super::record_units(extradata)
    } else {
        crate::nal::units(extradata, 0).collect()
    }
}

fn lost() -> Error {
    Error::Unsupported("no decoder left for this MV-HEVC video".into())
}

impl MergedDecoder {
    /// Wraps `inner`, an HEVC decoder opened for the rewritten stream, for
    /// a stream with parameters `par` (copied) and the rewriter of its
    /// configuration; `hw` and `software_ok` as the fields.
    pub(crate) fn new(
        inner: Decoder,
        rewriter: Rewriter,
        par: *const ff::AVCodecParameters,
        time_base: ff::AVRational,
        reversed: bool,
        hw: HwDecode,
        software_ok: bool,
    ) -> Result<MergedDecoder> {
        // SAFETY: copies the caller's parameters into an allocation of ours.
        let copy = unsafe {
            let mut p = ff::avcodec_parameters_alloc();
            if p.is_null() || ff::avcodec_parameters_copy(p, par) < 0 {
                ff::avcodec_parameters_free(&mut p);
                return Err(Error::Unsupported("codec parameters copy".into()));
            }
            p
        };
        // SAFETY: as above, now ours.
        let extradata = crate::mvc::extradata(unsafe { &*copy });
        let length_size = match extradata {
            [1, ..] => extradata.get(21).map_or(4, |&b| usize::from(b & 3) + 1),
            _ => 0,
        };
        Ok(MergedDecoder {
            inner_name: inner.name().to_string(),
            inner: Some(inner),
            rewriter,
            length_size,
            par: copy,
            time_base,
            reversed,
            hw,
            software_ok,
            interrupt: None,
            gave_up: None,
            fallback: None,
            shown: None,
            last_out: None,
            history: Vec::new(),
            history_whole: true,
            await_key: false,
            replay: VecDeque::new(),
            ended: false,
            end_owed: false,
            broken: 0,
            queue: VecDeque::new(),
            tags: HashMap::new(),
            next_tag: 0,
            aus: 0,
            bases: VecDeque::new(),
            seconds: HashMap::new(),
            scratch: Frame::new(),
            skip: None,
            skip_seconds: None,
        })
    }

    /// The decoder at work: of the rewritten stream, or the one playing on
    /// after it gave up.
    pub(crate) fn decoder(&self) -> Option<&Decoder> {
        self.fallback.as_ref().or(self.inner.as_ref())
    }

    /// As [`Decoder::set_interrupt`], for the decoder at work and any after
    /// it.
    pub(crate) fn set_interrupt(&mut self, flag: Arc<AtomicBool>) {
        if let Some(d) = self.fallback.as_mut().or(self.inner.as_mut()) {
            d.set_interrupt(flag.clone());
        }
        self.interrupt = Some(flag);
    }

    /// Whether the base view is the left eye's: as the stream's SEI says,
    /// else as the container does.
    fn base_left(&self) -> bool {
        let views = self.rewriter.view_ids();
        match self.rewriter.eyes {
            Some((left, _)) if left == views[0] => true,
            Some((_, right)) if right == views[0] => false,
            _ => !self.reversed,
        }
    }

    /// Goes on in software as if the rewrite had met what it can't express.
    #[cfg(test)]
    pub(crate) fn give_up_now(&mut self) {
        self.give_up("asked to".into()).unwrap();
    }

    /// Hands the decoder playing on the history it hasn't taken, then the
    /// end of the stream if that was sent before; false when it is full.
    fn feed_fallback(&mut self) -> Result<bool> {
        let f = self.fallback.as_mut().ok_or_else(lost)?;
        while let Some(p) = self.replay.front() {
            if !f.send(p.as_ptr())? {
                return Ok(false);
            }
            self.replay.pop_front();
        }
        if self.end_owed {
            if !f.send(std::ptr::null())? {
                return Ok(false);
            }
            self.end_owed = false;
        }
        Ok(true)
    }

    /// Sends a packet (null: the end), as [`Decoder::send`].
    pub(crate) fn send(&mut self, pkt: *const ff::AVPacket) -> Result<bool> {
        if self.fallback.is_some() {
            if !self.feed_fallback()? {
                return Ok(false);
            }
            if self.await_key && !pkt.is_null() {
                // SAFETY: a valid packet.
                if unsafe { (*pkt).flags } & ff::AV_PKT_FLAG_KEY as i32 == 0 {
                    return Ok(true);
                }
                self.await_key = false;
            }
            return self.fallback.as_mut().ok_or_else(lost)?.send(pkt);
        }
        if !self.send_queued()? {
            return Ok(false);
        }
        if pkt.is_null() {
            let sent = self.inner.as_mut().ok_or_else(lost)?.send(pkt)?;
            self.ended |= sent;
            self.inner_changed()?;
            return Ok(sent);
        }
        // SAFETY: a valid packet holding `size` bytes at `data`.
        let (data, pts, duration, key) = unsafe {
            let p = &*pkt;
            let data = if p.data.is_null() {
                &[][..]
            } else {
                std::slice::from_raw_parts(p.data, p.size.max(0) as usize)
            };
            let pts = if p.pts == ff::AV_NOPTS_VALUE {
                p.dts
            } else {
                p.pts
            };
            (
                data,
                pts,
                p.duration,
                p.flags & ff::AV_PKT_FLAG_KEY as i32 != 0,
            )
        };
        if key {
            self.history.clear();
            self.history_whole = true;
        } else if self.history.len() >= HISTORY {
            self.history.clear();
            self.history_whole = false;
        }
        if self.history_whole {
            let copy = Packet::new();
            // SAFETY: adds a reference to the caller's packet.
            if unsafe { ff::av_packet_ref(copy.as_ptr(), pkt) } >= 0 {
                self.history.push(copy);
            }
        }
        let au = self.aus;
        self.aus += 1;
        match self
            .rewriter
            .access_unit(crate::nal::units(data, self.length_size))
        {
            Ok(pictures) => {
                for p in pictures {
                    let tag = Tag {
                        pts,
                        duration,
                        au,
                        slot: p.slot,
                    };
                    self.queue_picture(p.data, tag, p.key)?;
                }
            }
            Err(Bad::Broken(why)) => {
                self.broken += 1;
                if self.broken >= BROKEN {
                    log::warn!("MV-HEVC: {why}, again; decoding on without the rewrite");
                    return self.give_up(why.into());
                }
                log::debug!("MV-HEVC: {why}; starting again at the next keyframe");
                self.rewriter.restart();
            }
            Err(Bad::Unsupported(why)) => {
                log::warn!("MV-HEVC: {why}; decoding on without the rewrite from here");
                return self.give_up(why);
            }
        }
        self.send_queued()?;
        self.inner_changed()?;
        Ok(true)
    }

    /// Goes on without the rewrite, from the last keyframe: both views in
    /// software, or the base view in both eyes when the CPU can't decode
    /// both in time.
    fn give_up(&mut self, why: String) -> Result<bool> {
        self.gave_up = Some(why);
        self.shown = self.last_out;
        // Closed first: its hardware session is the next decoder's to have.
        self.inner = None;
        self.queue.clear();
        self.tags.clear();
        self.bases.clear();
        self.seconds.clear();
        let mode = if self.software_ok {
            MvHevcMode::BothViews
        } else {
            MvHevcMode::BaseTwice(self.hw)
        };
        let mut f = Decoder::open_mv_hevc(self.par, self.time_base, mode, self.reversed)?;
        if let Some(flag) = &self.interrupt {
            f.set_interrupt(flag.clone());
        }
        if self.skip.is_some()
            && let Some((target, frame_duration)) = self.skip_seconds
        {
            f.skip_before(target, frame_duration);
        }
        self.replay = std::mem::take(&mut self.history).into();
        self.await_key = !self.history_whole;
        self.end_owed = self.ended;
        self.fallback = Some(f);
        Ok(true)
    }

    /// Gives up when the decoder of the rewritten stream fell back to
    /// another (the hardware decoder failed): that one starts in the middle
    /// of a stream whose parameter sets came at its start.
    fn inner_changed(&mut self) -> Result<()> {
        if self
            .inner
            .as_ref()
            .is_some_and(|i| i.name() != self.inner_name)
        {
            log::warn!("MV-HEVC: the hardware decoder failed; decoding on without the rewrite");
            self.give_up("the hardware decoder failed".into())?;
        }
        Ok(())
    }

    fn queue_picture(&mut self, data: Vec<u8>, tag: Tag, key: bool) -> Result<()> {
        let pkt = Packet::new();
        let t = self.next_tag;
        self.next_tag += 1;
        // SAFETY: a fresh packet given a buffer of data's size, filled.
        unsafe {
            let p = pkt.as_ptr();
            check(ff::av_new_packet(p, data.len() as i32), "MV-HEVC packet")?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), (*p).data, data.len());
            (*p).pts = t;
            (*p).dts = t;
            (*p).duration = tag.duration;
            if key {
                (*p).flags |= ff::AV_PKT_FLAG_KEY as i32;
            }
        }
        // The pictures made up aren't shown: they come back from no decoder
        // (and are dropped, any that do).
        if matches!(tag.slot, Slot::Base | Slot::Dependent) {
            self.tags.insert(t, tag);
        }
        // Tags of pictures the decoder never gave back (damaged ones).
        if self.tags.len() > 512 {
            self.tags.retain(|&k, _| k > t - 256);
        }
        self.queue.push_back(pkt);
        Ok(())
    }

    /// Hands queued pictures to the decoder; false when it is full.
    fn send_queued(&mut self) -> Result<bool> {
        let inner = self.inner.as_mut().ok_or_else(lost)?;
        while let Some(p) = self.queue.front() {
            if !inner.send(p.as_ptr())? {
                return Ok(false);
            }
            self.queue.pop_front();
        }
        Ok(true)
    }

    /// Receives a side-by-side frame, as [`Decoder::receive`].
    pub(crate) fn receive(&mut self, frame: &mut Frame) -> Result<Option<bool>> {
        if self.fallback.is_some() {
            loop {
                self.feed_fallback()?;
                let f = self.fallback.as_mut().ok_or_else(lost)?;
                let r = f.receive(frame)?;
                if r != Some(true) {
                    return Ok(r);
                }
                // SAFETY: a decoded frame.
                let pts = unsafe { (*frame.as_ptr()).pts };
                if self.shown.is_none_or(|s| pts > s) {
                    return Ok(r);
                }
            }
        }
        loop {
            if self.pair_ready(frame, false)? {
                return Ok(Some(true));
            }
            self.send_queued()?;
            let r = self
                .inner
                .as_mut()
                .ok_or_else(lost)?
                .receive(&mut self.scratch)?;
            self.inner_changed()?;
            if self.fallback.is_some() {
                return self.receive(frame);
            }
            match r {
                None => return Ok(None),
                Some(false) => {
                    // The end: base pictures still waiting go out, alone
                    // when their second views never came.
                    let paired = self.pair_ready(frame, true)?;
                    return Ok(Some(paired));
                }
                Some(true) => {}
            }
            // SAFETY: a decoded frame.
            let t = unsafe { (*self.scratch.as_ptr()).pts };
            let decoded = Frame(self.scratch.take_raw());
            let Some(tag) = self.tags.remove(&t) else {
                continue;
            };
            match tag.slot {
                Slot::Start | Slot::Copy => {}
                Slot::Base => self.bases.push_back((decoded, tag)),
                Slot::Dependent => {
                    self.seconds.insert(tag.au, decoded);
                }
            }
        }
    }

    /// Puts the oldest base picture with its second view (or, waited on
    /// long enough or at the end, alone) into `out`.
    fn pair_ready(&mut self, out: &mut Frame, at_end: bool) -> Result<bool> {
        while let Some((_, tag)) = self.bases.front() {
            let tag = *tag;
            let second = self.seconds.remove(&tag.au);
            if second.is_none() && !at_end && self.bases.len() <= WAITING {
                return Ok(false);
            }
            let (base, tag) = self.bases.pop_front().expect("checked");
            // Second views whose base pictures never came: the oldest go.
            while self.seconds.len() > 4 * WAITING {
                let oldest = *self.seconds.keys().min().expect("not empty");
                self.seconds.remove(&oldest);
            }
            let paired = match &second {
                Some(s) if self.base_left() => self.pair(out, &base, s, tag)?,
                Some(s) => self.pair(out, s, &base, tag)?,
                None => self.pair(out, &base, &base, tag)?,
            };
            if paired {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Puts `left` and `right` side by side into `out`, timed as `tag`;
    /// false when a precise seek leaves the pair out.
    fn pair(&mut self, out: &mut Frame, left: &Frame, right: &Frame, tag: Tag) -> Result<bool> {
        self.broken = 0;
        if let Some(s) = self.skip {
            if s.drops(tag.pts, tag.duration) {
                return Ok(false);
            }
            self.skip = None;
        }
        // SAFETY: both are decoded frames of the same size and format; out
        // is ours to fill.
        unsafe {
            side_by_side(out.as_ptr(), left.as_ptr(), right.as_ptr())?;
            let o = &mut *out.as_ptr();
            o.pts = tag.pts;
            o.best_effort_timestamp = tag.pts;
            o.duration = tag.duration;
        }
        self.last_out = Some(tag.pts);
        Ok(true)
    }

    pub(crate) fn flush(&mut self) {
        self.skip = None;
        self.ended = false;
        self.end_owed = false;
        if let Some(f) = self.fallback.as_mut() {
            self.shown = None;
            self.replay.clear();
            return f.flush();
        }
        if let Some(inner) = self.inner.as_mut() {
            inner.flush();
        }
        self.rewriter.restart();
        self.broken = 0;
        self.last_out = None;
        self.history.clear();
        self.history_whole = false;
        self.queue.clear();
        self.tags.clear();
        self.bases.clear();
        self.seconds.clear();
    }
}
