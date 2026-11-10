//! Rewrites two-view MV-HEVC into one single-layer HEVC stream that an
//! ordinary HEVC decoder (the Frame's hardware decoder among them) decodes
//! into the very pictures of both views.
//!
//! MV-HEVC changes nothing below the slice header: the second view's slices
//! decode as HEVC slices do, the base view's picture of their access unit
//! one more reference, which they treat as a long-term one. So each access
//! unit becomes three pictures, numbered in one stream:
//!
//! - the base view's, as it is but for its picture order count's length and
//!   its reference picture set, which also keeps the second view's pictures;
//! - a copy of it (every coding unit skipped, [`super::cabac`]), which the
//!   second view's picture marks long-term: the base picture itself has to
//!   stay a short-term reference for the base view's pictures after it;
//! - the second view's, its reference picture set holding its own pictures,
//!   the base view's, and the copy as its one long-term reference, and its
//!   reference lists spelled out to be what MV-HEVC builds.
//!
//! Each view keeps its picture order count differences, which motion vector
//! scaling depends on: the base view's pictures keep their counts (raised
//! by `3M`), the second view's are the base's less `M`, the copies' less
//! `2M`, `M` the largest LSB range of the two views, so no two pictures in
//! the decoded picture buffer at once share a count. (Their output order
//! is the stream's no longer, the second view's coming out early, which
//! the pairing doesn't mind.)
//!
//! Each coded video sequence of the rewritten stream starts with an IDR
//! picture of its own, not shown, made up ([`super::cabac`]): the base
//! view's IRAP picture would come after the copy and the second view's
//! picture of its own access unit in output order, as no trailing picture
//! may, so it and the leading pictures after it become trailing pictures.
//!
//! What the rewrite can't express makes the stream play in software: a
//! second view taking motion from the base view's picture (temporal motion
//! vector prediction from it), long-term references, views coded with
//! different tools.

use super::cabac::copy_slice_data;
use super::syntax::{self, NalHeader, Pps, Slice, Sps, Vps};
use crate::mvc::Bad;
use crate::mvc::bits::{Writer, escape_into, stop_bit, unescape};
use std::collections::HashMap;

/// What a picture of the rewritten stream is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    /// The intra picture a coded video sequence of the rewritten stream
    /// starts with, not shown.
    Start,
    Base,
    /// A copy of the base picture, not shown.
    Copy,
    /// The second view's.
    Dependent,
}

/// One picture of the rewritten stream, Annex B.
pub(crate) struct Picture {
    pub(crate) data: Vec<u8>,
    pub(crate) slot: Slot,
    pub(crate) key: bool,
}

/// A short-term reference picture set to write: the pictures before the
/// current one (closest first) and after, as POC differences, and whether
/// it predicts from each.
pub(crate) type Rps = (Vec<(i32, bool)>, Vec<(i32, bool)>);

/// The second view's PPS IDs are its own plus this.
const LAYER1_PPS: u32 = 32;
/// The copies' PPS.
const COPY_PPS: u32 = 63;

struct Stored<T> {
    value: T,
    layer: u8,
    rbsp: Vec<u8>,
}

/// A slice segment of the access unit being rewritten.
struct Segment {
    nal: Vec<u8>,
    header: NalHeader,
    rbsp: Vec<u8>,
    slice: Slice,
}

/// A reference picture of the second view's slice, as MV-HEVC lists them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ref {
    Own(i32),
    InterLayer,
}

pub(crate) struct Rewriter {
    vps: HashMap<u32, Stored<Vps>>,
    sps: HashMap<u32, Stored<Sps>>,
    pps: HashMap<u32, Stored<Pps>>,
    /// Pictures a second of the rewritten stream (three per access unit).
    rate: f64,
    /// Parameter sets changed since they were last sent.
    resend: bool,
    /// Since the last restart, an IRAP picture was seen.
    started: bool,
    /// The next picture starts a coded video sequence (after an end of
    /// sequence).
    new_sequence: bool,
    /// Started at a CRA picture: its RASL pictures can't decode.
    skip_rasl: bool,
    /// The base view's previous TemporalId 0 picture's POC.
    prev_tid0: i32,
    /// What the current coded video sequence's POCs are raised by: all of
    /// its pictures' above the IDR picture it starts with.
    offset: i32,
    /// The base view's pictures there are to refer to (by their new POCs):
    /// the last one and what it kept.
    base_keep: Vec<i32>,
    /// The second view's pictures to keep (by their new POCs) through an
    /// access unit without one of its pictures.
    layer1_keep: Vec<i32>,
    /// The views a three-dimensional reference displays information SEI
    /// names the left and the right eye's.
    pub(crate) eyes: Option<(u32, u32)>,
}

impl Rewriter {
    /// `fps`: the video's frame rate.
    pub(crate) fn new(fps: f64) -> Rewriter {
        Rewriter {
            vps: HashMap::new(),
            sps: HashMap::new(),
            pps: HashMap::new(),
            rate: fps * 3.0,
            resend: true,
            started: false,
            new_sequence: true,
            skip_rasl: false,
            prev_tid0: 0,
            offset: 0,
            base_keep: Vec::new(),
            layer1_keep: Vec::new(),
            eyes: None,
        }
    }

    /// The views' IDs, base layer's first.
    pub(crate) fn view_ids(&self) -> [u32; 2] {
        self.active_vps().map_or([0, 1], |v| v.value.view_ids)
    }

    /// The base view's bit depth (8 before its SPS came).
    pub(crate) fn bit_depth(&self) -> u32 {
        self.sps.values().find(|s| s.layer == 0).map_or(8, |s| {
            let rep = &s.value.tools.rep;
            rep.bit_depth_luma.max(rep.bit_depth_chroma)
        })
    }

    /// The second layer's nuh_layer_id.
    fn layer1(&self) -> Option<u8> {
        self.active_vps().and_then(|v| v.value.layer1)
    }

    /// The VPS the base view's SPS names, else any there is.
    fn active_vps(&self) -> Option<&Stored<Vps>> {
        let base = self.sps.values().find(|s| s.layer == 0);
        base.and_then(|s| s.rbsp.get(2))
            .and_then(|b| self.vps.get(&u32::from(b >> 4)))
            .or_else(|| self.vps.values().next())
    }

    /// Starts again at the next IRAP picture (after a seek, or damage).
    pub(crate) fn restart(&mut self) {
        self.started = false;
        self.new_sequence = true;
        self.resend = true;
        self.layer1_keep.clear();
    }

    /// Takes in a parameter set (or SEI) of the configuration or a packet.
    pub(crate) fn parameter_set(&mut self, nal: &[u8]) -> Result<(), Bad> {
        let Some(h) = NalHeader::parse(nal) else {
            return Ok(());
        };
        let rbsp = unescape(nal);
        match h.kind {
            syntax::VPS if h.layer == 0 => {
                let value = syntax::vps(&rbsp)?;
                self.store_vps(value, h.layer, rbsp);
            }
            syntax::SPS => {
                let vps_id = u32::from(rbsp.get(2).ok_or(Bad::Broken("SPS cut short"))? >> 4);
                let vps = self.vps.get(&vps_id).map(|v| &v.value);
                let value = syntax::sps(&rbsp, h.layer, vps)?;
                if self.sps.get(&value.id).is_none_or(|s| s.rbsp != rbsp) {
                    self.resend = true;
                    self.sps.insert(
                        value.id,
                        Stored {
                            value,
                            layer: h.layer,
                            rbsp,
                        },
                    );
                }
            }
            syntax::PPS => {
                let value = syntax::pps(&rbsp, |id| self.sps.get(&id).map(|s| &s.value))?;
                if value.id >= LAYER1_PPS - 1 {
                    return Err(Bad::Unsupported("PPS IDs from 31 on".into()));
                }
                if self.pps.get(&value.id).is_none_or(|s| s.rbsp != rbsp) {
                    self.resend = true;
                    self.pps.insert(
                        value.id,
                        Stored {
                            value,
                            layer: h.layer,
                            rbsp,
                        },
                    );
                }
            }
            39 if h.layer == 0 => {
                if let Some(eyes) = syntax::eye_views(&rbsp) {
                    self.eyes = Some(eyes);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Whether the configuration can be rewritten: both views' SPSs there
    /// and coded alike, the merged parameter sets written.
    pub(crate) fn check(&self) -> Result<(), Bad> {
        self.layer1()
            .ok_or(Bad::Unsupported("no second view in the VPS".into()))?;
        let base = self
            .sps
            .values()
            .find(|s| s.layer == 0)
            .ok_or(Bad::Broken("no SPS"))?;
        let l1 = self
            .sps
            .values()
            .find(|s| s.layer > 0)
            .ok_or(Bad::Unsupported("no SPS of the second view".into()))?;
        alike(&base.value, &l1.value)?;
        self.parameter_sets(base.value.id, &mut Vec::new())
    }

    fn store_vps(&mut self, value: Vps, layer: u8, rbsp: Vec<u8>) {
        if self.vps.get(&value.id).is_none_or(|s| s.rbsp != rbsp) {
            self.resend = true;
            self.vps.insert(value.id, Stored { value, layer, rbsp });
        }
    }

    /// Rewrites an access unit's NAL units into the pictures of the merged
    /// stream: none while waiting for an IRAP picture to start at.
    pub(crate) fn access_unit<'a>(
        &mut self,
        nals: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<Vec<Picture>, Bad> {
        let layer1 = self.layer1().ok_or(Bad::Unsupported(
            "no second view in the parameter sets".into(),
        ))?;
        let mut base_nals = Vec::new();
        let mut dep_nals = Vec::new();
        let mut prefix = Vec::new();
        let mut end = Vec::new();
        for nal in nals {
            let Some(h) = NalHeader::parse(nal) else {
                continue;
            };
            match h.kind {
                syntax::VPS | syntax::SPS | syntax::PPS => self.parameter_set(nal)?,
                39 if h.layer == 0 => {
                    self.parameter_set(nal)?;
                    prefix.push(nal);
                }
                36 | 37 if h.layer == 0 => end.push(nal),
                k if k < 32 && h.layer == 0 => base_nals.push((h, nal)),
                k if k < 32 && h.layer == layer1 => dep_nals.push((h, nal)),
                _ => {}
            }
        }
        let Some(&(bh, _)) = base_nals.first() else {
            return Ok(Vec::new());
        };
        if !self.started && !bh.irap() {
            return Ok(Vec::new());
        }
        let first_of_sequence = self.new_sequence || bh.idr() || bh.bla();
        if first_of_sequence {
            // A CRA picture starting a sequence, or a BLA picture: the RASL
            // pictures after it can't decode.
            self.skip_rasl = bh.kind == 21 || bh.bla();
        } else if bh.irap() {
            self.skip_rasl = false;
        }
        if bh.rasl() && self.skip_rasl {
            return Ok(Vec::new());
        }
        let base = self.segments(&base_nals)?;
        let dep = self.segments(&dep_nals)?;
        let b = &base[0].slice;
        let base_sps_id = self.pps[&b.pps_id].value.sps_id;
        let base_sps = self.sps[&base_sps_id].value.clone();
        let l1_sps = dep.first().map(|d| {
            let pps = &self.pps[&d.slice.pps_id].value;
            self.sps[&pps.sps_id].value.clone()
        });
        let l1_sps = l1_sps.as_ref();
        let max = 1u32 << base_sps.log2_max_poc_lsb;
        let original = match b.poc_lsb {
            None => 0,
            Some(lsb) if bh.irap() && first_of_sequence => lsb as i32,
            Some(lsb) => syntax::poc(self.prev_tid0, lsb, max, bh.bla()),
        };
        if bh.tid == 0 && !bh.rasl() && !matches!(bh.kind, 6 | 7) && !bh.sub_layer_non_reference() {
            self.prev_tid0 = original;
        }
        self.started = true;
        self.new_sequence = !end.is_empty();
        if let Some(l) = l1_sps {
            alike(&base_sps, l)?;
        }
        // The second view's SPS of this access unit, else the one known: M
        // is the same through a sequence, with or without its pictures.
        let log2 = l1_sps
            .map(|l| l.log2_max_poc_lsb)
            .or_else(|| {
                let l1 = self.sps.values().find(|s| s.layer > 0);
                l1.map(|s| s.value.log2_max_poc_lsb)
            })
            .unwrap_or(0)
            .max(base_sps.log2_max_poc_lsb);
        let m = 1i32 << log2;
        if first_of_sequence {
            // Above the IDR picture's 0 even for the copies of leading
            // pictures (at least the IRAP picture's less M / 2), and below
            // 32768 for the IRAP picture (less than M).
            self.offset = 3 * m;
            self.base_keep.clear();
            self.layer1_keep.clear();
        }
        let p = original + self.offset;
        let (e_poc, c_poc) = (p - m, p - 2 * m);

        // The base view's references, and the second view's: of those a
        // picture names, the ones there are. One it doesn't predict from
        // that isn't there (left behind by a seek, or before the sequence)
        // is left out, as is any of a picture of I slices, which name none
        // in their headers.
        let intra = |s: &[Segment]| s.iter().all(|s| s.slice.slice_type == 2);
        let (base_intra, dep_intra) = (intra(&base), intra(&dep));
        let there = |rps: &syntax::StRps, poc: i32, kept: &[i32], all_intra: bool| {
            let keep = |e: &(i32, bool)| kept.contains(&(poc + e.0)) || (e.1 && !all_intra);
            syntax::StRps {
                before: rps.before.iter().copied().filter(keep).collect(),
                after: rps.after.iter().copied().filter(keep).collect(),
            }
        };
        let base_rps = b.st_rps.clone().unwrap_or_default();
        let base_rps = there(&base_rps, p, &self.base_keep, base_intra);
        let base_keep: Vec<i32> = base_rps
            .all()
            .map(|(d, _)| p + d)
            .chain(std::iter::once(p))
            .collect();
        self.base_keep.clone_from(&base_keep);
        let d0 = dep.first().map(|d| &d.slice);
        if let Some(d) = d0
            && let Some(lsb) = d.poc_lsb
            && l1_sps.is_some_and(|l| (original.rem_euclid(1 << l.log2_max_poc_lsb)) as u32 != lsb)
        {
            return Err(Bad::Unsupported(
                "views of one picture with different POCs".into(),
            ));
        }
        let dep_rps = d0.and_then(|d| d.st_rps.clone()).unwrap_or_default();
        let dep_rps = there(&dep_rps, e_poc, &self.layer1_keep, dep_intra);
        if first_of_sequence && dep_rps.all().next().is_some() {
            // The IDR picture the rewritten sequence starts with leaves
            // none of the second view's pictures to predict from.
            return Err(Bad::Unsupported(
                "a second view predicted across the start of a sequence".into(),
            ));
        }
        let l1_refs: Vec<i32> = if d0.is_some() {
            dep_rps.all().map(|(d, _)| e_poc + d).collect()
        } else {
            self.layer1_keep.clone()
        };
        if d0.is_some() {
            self.layer1_keep = l1_refs.iter().copied().chain([e_poc]).collect();
        }
        let copy = d0.is_some_and(|d| d.inter_layer) && !dep_intra;

        let mut out = Vec::new();
        let sps = base_sps;
        let shape = Shape::merged(&sps);
        let mut data = Vec::new();
        if self.resend || first_of_sequence {
            self.parameter_sets(base_sps_id, &mut data)?;
            self.resend = false;
        }
        if first_of_sequence {
            data.extend(starter_picture(&shape, false));
            out.push(Picture {
                data: std::mem::take(&mut data),
                slot: Slot::Start,
                key: true,
            });
        }
        // The base picture: a trailing one that may be referred to, as its
        // copy does.
        for nal in &prefix {
            annex_b(nal, &mut data);
        }
        let mut rps: Vec<(i32, bool)> = base_rps.all().collect();
        rps.extend(l1_refs.iter().map(|&q| (q - p, false)));
        let rps = split(&rps)?;
        for s in &base {
            let mut w = Writer::new();
            w.extend(
                &NalHeader {
                    kind: syntax::TRAIL_R,
                    layer: 0,
                    tid: s.header.tid,
                }
                .bytes(),
            );
            w.flag(s.slice.first);
            // From slice_pic_parameter_set_id on (past an IRAP picture's
            // no_output_of_prior_pics_flag).
            let from = 16 + 1 + usize::from(s.header.irap());
            if s.slice.dependent {
                w.copy(&s.rbsp, from, s.slice.extension.1);
            } else {
                w.copy(&s.rbsp, from, s.slice.poc_at);
                w.u(16, (p as u32) & 0xffff);
                write_rps(&mut w, &rps);
                w.ue(0);
                // slice_temporal_mvp_enabled_flag, which a picture of I
                // slices doesn't use: 1, as 0 would let the decoder forget
                // the motion the other view's pictures still predict from.
                let mut rest = s.slice.after_refs;
                if sps.temporal_mvp && (s.header.idr() || base_intra) {
                    w.flag(true);
                    rest += usize::from(!s.header.idr());
                }
                w.copy(&s.rbsp, rest, s.slice.extension.1);
            }
            slice_out(w, s, &mut data);
        }
        out.push(Picture {
            data,
            slot: Slot::Base,
            key: bh.irap(),
        });

        if copy {
            let kind = syntax::TRAIL_R;
            let mut rps = vec![(p - c_poc, true)];
            rps.extend(
                base_keep[..base_keep.len() - 1]
                    .iter()
                    .map(|&q| (q - c_poc, false)),
            );
            rps.extend(l1_refs.iter().map(|&q| (q - c_poc, false)));
            out.push(Picture {
                data: copy_picture(&shape, kind, bh.tid, c_poc, &split(&rps)?, false),
                slot: Slot::Copy,
                key: false,
            });
        }

        // The second view's picture.
        if !dep.is_empty() {
            let mut rps: Vec<(i32, bool)> = dep_rps.all().collect();
            rps.extend(base_keep.iter().map(|&q| (q - e_poc, false)));
            let rps = split(&rps)?;
            let mut data = Vec::new();
            for s in &dep {
                let mut w = Writer::new();
                let h = NalHeader {
                    kind: if s.header.sub_layer_non_reference() {
                        syntax::TRAIL_N
                    } else {
                        syntax::TRAIL_R
                    },
                    layer: 0,
                    tid: s.header.tid,
                };
                w.extend(&h.bytes());
                w.flag(s.slice.first);
                w.ue(s.slice.pps_id + LAYER1_PPS);
                w.copy(&s.rbsp, s.slice.address.0, s.slice.address.1);
                if !s.slice.dependent {
                    dependent_header(
                        &mut w,
                        s,
                        &sps,
                        &rps,
                        (e_poc, copy.then_some(c_poc)),
                        &dep_rps,
                        dep_intra,
                    )?;
                }
                w.copy(&s.rbsp, s.slice.entry.0, s.slice.entry.1);
                slice_out(w, s, &mut data);
            }
            data.extend(end.iter().flat_map(|n| {
                let mut v = Vec::new();
                annex_b(n, &mut v);
                v
            }));
            out.push(Picture {
                data,
                slot: Slot::Dependent,
                key: false,
            });
        } else if let Some(last) = out.last_mut() {
            for n in &end {
                annex_b(n, &mut last.data);
            }
        }
        Ok(out)
    }

    fn segments(&self, nals: &[(NalHeader, &[u8])]) -> Result<Vec<Segment>, Bad> {
        let mut out: Vec<Segment> = Vec::new();
        for &(header, nal) in nals {
            let rbsp = unescape(nal);
            let mut r = crate::mvc::bits::Reader::new(&rbsp, 16);
            r.flag()?;
            if header.irap() {
                r.flag()?;
            }
            let pps = &self
                .pps
                .get(&r.ue()?)
                .ok_or(Bad::Broken("slice before its PPS"))?
                .value;
            let sps = &self
                .sps
                .get(&pps.sps_id)
                .ok_or(Bad::Broken("slice before its SPS"))?
                .value;
            let vps = self.active_vps().map(|v| &v.value);
            let previous = out
                .iter()
                .rev()
                .find(|s| !s.slice.dependent)
                .map(|s| &s.slice);
            let slice = syntax::slice(&rbsp, header, pps, sps, vps, previous)?;
            if out.is_empty() && (!slice.first || slice.dependent) {
                return Err(Bad::Broken("a picture without its first slice"));
            }
            if !out.is_empty() && slice.first {
                return Err(Bad::Unsupported(
                    "several pictures of a view in a packet".into(),
                ));
            }
            out.push(Segment {
                nal: nal.to_vec(),
                header,
                rbsp,
                slice,
            });
        }
        Ok(out)
    }

    /// The parameter sets of the merged stream, Annex B: the VPS, the SPS
    /// both views share, the base view's PPSs as they are, the second
    /// view's (renumbered) and the copies'.
    fn parameter_sets(&self, sps_id: u32, out: &mut Vec<u8>) -> Result<(), Bad> {
        let base = self.sps.get(&sps_id).ok_or(Bad::Broken("no SPS"))?;
        let s = &base.value;
        let l1 = self
            .sps
            .values()
            .find(|x| x.layer > 0)
            .map(|x| &x.value)
            .ok_or(Bad::Unsupported("no SPS of the second view".into()))?;
        let dec = s.dpb.max_dec_pic_buffering + l1.dpb.max_dec_pic_buffering.max(1) + 1;
        if dec > 16 {
            return Err(Bad::Unsupported("too many reference pictures".into()));
        }
        let reorder = (s.dpb.num_reorder_pics + l1.dpb.num_reorder_pics + 1).min(dec - 1);
        let rep = &s.tools.rep;
        let level = level(
            base.rbsp[s.level_at / 8],
            self.rate * f64::from(rep.width) * f64::from(rep.height),
            u64::from(rep.width) * u64::from(rep.height),
            dec,
        );
        let ordering = |w: &mut Writer| {
            w.flag(false);
            w.ue(dec - 1);
            w.ue(reorder);
            w.ue(0);
        };
        // The VPS.
        let vps = self.active_vps().ok_or(Bad::Broken("no VPS"))?;
        let v = &vps.value;
        let mut w = Writer::new();
        w.extend(&[0x40, 0x01]);
        w.u(4, v.id);
        w.u(2, 3);
        w.u(6, 0);
        w.u(3, v.max_sub_layers_minus1);
        // Nesting only where it has to be on (one sub-layer): with it, a
        // picture of a higher sub-layer could not be a trailing one.
        w.flag(v.max_sub_layers_minus1 == 0);
        w.u(16, 0xffff);
        w.copy(&vps.rbsp, v.ptl.0, v.level_at);
        w.u(8, u32::from(level));
        w.copy(&vps.rbsp, v.level_at + 8, v.ptl.1);
        ordering(&mut w);
        w.u(6, 0);
        w.ue(0);
        w.flag(false);
        w.flag(false);
        w.trailing();
        annex_b_rbsp(&w.into_bytes(), out);
        // The SPS.
        let stop = stop_bit(&base.rbsp).ok_or(Bad::Broken("SPS without a stop bit"))?;
        let mut w = Writer::new();
        // sps_temporal_id_nesting_flag as the VPS's.
        w.copy(&base.rbsp, 0, 23);
        w.flag(v.max_sub_layers_minus1 == 0);
        w.copy(&base.rbsp, 24, s.level_at);
        w.u(8, u32::from(level));
        w.copy(&base.rbsp, s.level_at + 8, s.poc_bits.0);
        w.ue(16 - 4);
        ordering(&mut w);
        w.copy(&base.rbsp, s.ordering.1, s.refs.0);
        w.ue(0);
        w.flag(true);
        w.ue(0);
        w.copy(&base.rbsp, s.refs.1, stop);
        w.trailing();
        annex_b_rbsp(&w.into_bytes(), out);
        // The PPSs.
        let mut ids: Vec<_> = self.pps.keys().copied().collect();
        ids.sort_unstable();
        for id in &ids {
            let p = &self.pps[id];
            if p.layer == 0 && p.value.sps_id == sps_id {
                annex_b(&escape(&p.rbsp), out);
            }
        }
        for id in &ids {
            let p = &self.pps[id];
            let v = &p.value;
            let mut w = Writer::new();
            w.extend(&[0x44, 0x01]);
            w.ue(v.id + LAYER1_PPS);
            w.ue(sps_id);
            w.copy(&p.rbsp, v.after_ids, v.lists_at);
            w.flag(true);
            w.copy(&p.rbsp, v.lists_at + 1, v.extension_at);
            w.flag(false);
            match v.range {
                Some((from, to)) => {
                    w.flag(true);
                    w.flag(true);
                    w.u(7, 0);
                    w.copy(&p.rbsp, from, to);
                }
                None => w.flag(false),
            }
            w.trailing();
            annex_b_rbsp(&w.into_bytes(), out);
        }
        out.extend(copy_pps(sps_id));
        Ok(())
    }
}

/// What of an SPS the pictures made up depend on.
pub(crate) struct Shape {
    pub(crate) log2_max_poc_lsb: u32,
    pub(crate) st_rps_sets: usize,
    /// num_long_term_ref_pics_sps, when long-term references are on.
    pub(crate) long_term: Option<u32>,
    pub(crate) temporal_mvp: bool,
    pub(crate) sao: bool,
    pub(crate) chroma: bool,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) log2_ctb: u32,
    pub(crate) log2_min_cb: u32,
    pub(crate) log2_min_tb: u32,
    pub(crate) log2_max_tb: u32,
    pub(crate) max_depth_intra: u32,
}

impl Shape {
    pub(crate) fn of(sps: &Sps) -> Shape {
        Shape {
            log2_max_poc_lsb: sps.log2_max_poc_lsb,
            st_rps_sets: sps.st_rps.len(),
            long_term: sps.long_term.then_some(sps.lt_sps),
            temporal_mvp: sps.temporal_mvp,
            sao: sps.sao,
            chroma: sps.chroma(),
            width: sps.tools.rep.width,
            height: sps.tools.rep.height,
            log2_ctb: sps.log2_ctb,
            log2_min_cb: sps.log2_min_cb,
            log2_min_tb: sps.tools.coding[2] + 2,
            log2_max_tb: sps.tools.coding[2] + 2 + sps.tools.coding[3],
            max_depth_intra: sps.tools.coding[5],
        }
    }

    /// The merged SPS's, built on the base view's `sps`.
    fn merged(sps: &Sps) -> Shape {
        Shape {
            log2_max_poc_lsb: 16,
            st_rps_sets: 0,
            long_term: Some(0),
            ..Shape::of(sps)
        }
    }
}

/// The PPS of the pictures made up, Annex B: one reference, no deblocking,
/// pic_output_flag in slice headers.
pub(crate) fn copy_pps(sps_id: u32) -> Vec<u8> {
    let mut w = Writer::new();
    w.extend(&[0x44, 0x01]);
    w.ue(COPY_PPS);
    w.ue(sps_id);
    w.flag(false); // dependent slices
    w.flag(true); // output_flag_present_flag
    w.u(4, 0); // extra bits, sign hiding
    w.flag(false); // cabac_init_present_flag
    w.ue(0);
    w.ue(0);
    w.se(0);
    w.u(3, 0); // constrained intra, transform skip, cu_qp_delta
    w.se(0);
    w.se(0);
    w.u(7, 0); // chroma offsets, weighted, bypass, tiles, WPP, across slices
    w.flag(true); // deblocking_filter_control_present_flag
    w.flag(false);
    w.flag(true); // pps_deblocking_filter_disabled_flag
    w.u(2, 0); // scaling list, lists modification
    w.ue(0);
    w.u(2, 0); // header extension, PPS extension
    w.trailing();
    let mut out = Vec::new();
    annex_b_rbsp(&w.into_bytes(), &mut out);
    out
}

/// An IDR picture to start a coded video sequence with, Annex B: one I
/// slice, of whatever the decoder makes of intra prediction from nothing.
pub(crate) fn starter_picture(shape: &Shape, shown: bool) -> Vec<u8> {
    let mut w = Writer::new();
    w.extend(
        &NalHeader {
            kind: syntax::IDR_N_LP,
            layer: 0,
            tid: 0,
        }
        .bytes(),
    );
    w.flag(true);
    w.flag(false); // no_output_of_prior_pics_flag
    w.ue(COPY_PPS);
    w.ue(2);
    w.flag(shown);
    if shape.sao {
        w.flag(false);
        if shape.chroma {
            w.flag(false);
        }
    }
    w.se(0);
    align(&mut w);
    w.extend(&super::cabac::intra_slice_data(&super::cabac::Blocks {
        width: shape.width,
        height: shape.height,
        log2_ctb: shape.log2_ctb,
        log2_min_cb: shape.log2_min_cb,
        log2_min_tb: shape.log2_min_tb,
        log2_max_tb: shape.log2_max_tb,
        max_depth_intra: shape.max_depth_intra,
        chroma: shape.chroma,
    }));
    let mut out = Vec::new();
    annex_b_rbsp(&w.into_bytes(), &mut out);
    out
}

/// A picture copying the first picture after it (`rps`'s first used one)
/// whole, Annex B: one P slice, every coding unit skipped.
pub(crate) fn copy_picture(
    shape: &Shape,
    kind: u8,
    tid: u8,
    poc: i32,
    rps: &Rps,
    shown: bool,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.extend(
        &NalHeader {
            kind,
            layer: 0,
            tid,
        }
        .bytes(),
    );
    w.flag(true);
    w.ue(COPY_PPS);
    w.ue(syntax::P);
    w.flag(shown);
    let bits = shape.log2_max_poc_lsb;
    w.u(bits, (poc as u32) & ((1 << bits) - 1));
    w.flag(false);
    if shape.st_rps_sets > 0 {
        w.flag(false); // inter_ref_pic_set_prediction_flag
    }
    write_rps_body(&mut w, rps);
    if let Some(sps_pictures) = shape.long_term {
        if sps_pictures > 0 {
            w.ue(0);
        }
        w.ue(0);
    }
    if shape.temporal_mvp {
        // On: off, in a picture of TemporalId 0, would let the decoder forget
        // the motion both views' pictures still predict from.
        w.flag(true);
    }
    if shape.sao {
        w.flag(false);
        if shape.chroma {
            w.flag(false);
        }
    }
    w.flag(true); // num_ref_idx_active_override_flag
    w.ue(0);
    w.ue(3); // five_minus_max_num_merge_cand: two, see copy_slice_data
    w.se(0);
    align(&mut w);
    w.extend(&copy_slice_data(
        shape.width,
        shape.height,
        shape.log2_ctb,
        shape.log2_min_cb,
    ));
    let mut out = Vec::new();
    annex_b_rbsp(&w.into_bytes(), &mut out);
    out
}

/// The header of an independent slice segment of the second view, from
/// slice_reserved_flag to the end of the loop filter flags.
/// `pocs`: the picture's, and its copy picture's when it has one. `own`:
/// its own view's pictures it refers to; `intra`: all its slices are I.
fn dependent_header(
    w: &mut Writer,
    s: &Segment,
    sps: &Sps,
    rps: &Rps,
    pocs: (i32, Option<i32>),
    own: &syntax::StRps,
    intra: bool,
) -> Result<(), Bad> {
    let (e_poc, copy) = pocs;
    let sl = &s.slice;
    w.copy(&s.rbsp, sl.reserved.0, sl.reserved.1);
    w.ue(sl.slice_type);
    if let Some(o) = sl.pic_output {
        w.flag(o);
    }
    w.u(16, (e_poc as u32) & 0xffff);
    write_rps(w, rps);
    match copy {
        Some(c) => {
            w.ue(1);
            w.u(16, (c as u32) & 0xffff);
            w.flag(true);
            w.flag(false);
        }
        None => w.ue(0),
    }
    if sps.temporal_mvp {
        // 1 for a picture of I slices: see the base view's.
        w.flag(sl.temporal_mvp || intra);
    }
    w.copy(&s.rbsp, sl.sao.0, sl.sao.1);
    if sl.slice_type != 2 {
        let lists = if sl.slice_type == syntax::B { 2 } else { 1 };
        w.flag(true);
        for l in 0..lists {
            w.ue(sl.num_ref_idx[l] - 1);
        }
        let before: Vec<i32> = own.before.iter().filter(|e| e.1).map(|e| e.0).collect();
        let after: Vec<i32> = own.after.iter().filter(|e| e.1).map(|e| e.0).collect();
        let il = sl.inter_layer;
        let total = (before.len() + after.len()) as u32 + u32::from(il);
        let il_list: &[Ref] = if il { &[Ref::InterLayer] } else { &[] };
        let own_refs = |v: &[i32]| v.iter().map(|&d| Ref::Own(d)).collect::<Vec<_>>();
        let (b, a) = (own_refs(&before), own_refs(&after));
        let original = [
            [b.as_slice(), il_list, a.as_slice()].concat(),
            [a.as_slice(), b.as_slice(), il_list].concat(),
        ];
        let merged = [
            [b.as_slice(), a.as_slice(), il_list].concat(),
            [a.as_slice(), b.as_slice(), il_list].concat(),
        ];
        let mut finals: [Vec<Ref>; 2] = Default::default();
        for l in 0..lists {
            let n = sl.num_ref_idx[l] as usize;
            let temp = &original[l];
            finals[l] = (0..n)
                .map(|i| {
                    let at = sl.list_entry[l].as_ref().map_or(i, |e| e[i] as usize);
                    temp.get(at % temp.len().max(1))
                        .copied()
                        .ok_or(Bad::Broken("reference index"))
                })
                .collect::<Result<_, _>>()?;
        }
        if let Some((l, idx)) = sl.collocated
            && finals[l].get(idx as usize) == Some(&Ref::InterLayer)
        {
            return Err(Bad::Unsupported(
                "motion predicted from the other view".into(),
            ));
        }
        if total > 1 {
            let bits = 32 - (total - 1).leading_zeros();
            for l in 0..lists {
                w.flag(true);
                for r in &finals[l] {
                    let at = merged[l]
                        .iter()
                        .position(|m| m == r)
                        .ok_or(Bad::Broken("reference missing"))?;
                    w.u(bits, at as u32);
                }
            }
        }
    }
    w.copy(&s.rbsp, sl.tail.0, sl.tail.1);
    Ok(())
}

/// Whether two views' SPSs decode their slices alike (conformance cropping
/// aside), with picture order counts short enough to number both views.
fn alike(a: &Sps, b: &Sps) -> Result<(), Bad> {
    if a.tools != b.tools {
        return Err(Bad::Unsupported("views coded differently".into()));
    }
    // The IDR pictures starting the rewritten stream are coded for 4:2:0
    // (and 4:0:0).
    if a.tools.rep.chroma_format_idc > 1 {
        return Err(Bad::Unsupported("chroma other than 4:2:0".into()));
    }
    if a.log2_max_poc_lsb.max(b.log2_max_poc_lsb) > 13 {
        return Err(Bad::Unsupported("picture order counts this long".into()));
    }
    Ok(())
}

/// Sorts a reference picture set's entries into those before (closest
/// first) and after the current picture.
pub(crate) fn split(entries: &[(i32, bool)]) -> Result<Rps, Bad> {
    let mut before: Vec<_> = entries.iter().copied().filter(|e| e.0 < 0).collect();
    let mut after: Vec<_> = entries.iter().copied().filter(|e| e.0 > 0).collect();
    before.sort_by_key(|e| -e.0);
    after.sort_by_key(|e| e.0);
    let distinct = |v: &[(i32, bool)]| v.windows(2).all(|p| p[0].0 != p[1].0);
    if entries.iter().any(|e| e.0 == 0 || e.0.abs() > 32768)
        || !distinct(&before)
        || !distinct(&after)
        || before.len() + after.len() > 16
    {
        return Err(Bad::Unsupported(
            "pictures of both views in one place".into(),
        ));
    }
    Ok((before, after))
}

/// short_term_ref_pic_set_sps_flag (0) and an explicit st_ref_pic_set, for
/// an SPS without sets of its own.
fn write_rps(w: &mut Writer, rps: &Rps) {
    w.flag(false);
    write_rps_body(w, rps);
}

/// An explicit st_ref_pic_set from num_negative_pics on.
fn write_rps_body(w: &mut Writer, (before, after): &Rps) {
    w.ue(before.len() as u32);
    w.ue(after.len() as u32);
    let mut prev = 0;
    for &(d, used) in before {
        w.ue((prev - d - 1) as u32);
        w.flag(used);
        prev = d;
    }
    prev = 0;
    for &(d, used) in after {
        w.ue((d - prev - 1) as u32);
        w.flag(used);
        prev = d;
    }
}

fn align(w: &mut Writer) {
    w.flag(true);
    while !w.len().is_multiple_of(8) {
        w.flag(false);
    }
}

/// A rewritten slice header (all of it but byte_alignment()), then the
/// slice's own data.
fn slice_out(mut w: Writer, s: &Segment, out: &mut Vec<u8>) {
    align(&mut w);
    let header = w.into_bytes();
    out.extend_from_slice(&[0, 0, 0, 1]);
    escape_into(&header, out);
    out.extend_from_slice(&s.nal[escaped_offset(&s.nal, s.slice.data)..]);
}

/// Where byte `at` of a NAL unit's RBSP is in the unit itself.
fn escaped_offset(nal: &[u8], at: usize) -> usize {
    let mut zeros = 0;
    let mut kept = 0;
    for (i, &b) in nal.iter().enumerate() {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        if kept == at {
            return i;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        kept += 1;
    }
    nal.len()
}

fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    escape_into(rbsp, &mut out);
    out
}

fn annex_b(nal: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
}

fn annex_b_rbsp(rbsp: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    escape_into(rbsp, out);
}

/// The lowest general_level_idc from `at_least` on whose limits hold this
/// many luma samples a second, pictures of this size, and `dpb` of them.
fn level(at_least: u8, rate: f64, size: u64, dpb: u32) -> u8 {
    const LEVELS: [(u8, u64, f64); 13] = [
        (30, 36_864, 552_960.0),
        (60, 122_880, 3_686_400.0),
        (63, 245_760, 7_372_800.0),
        (90, 552_960, 16_588_800.0),
        (93, 983_040, 33_177_600.0),
        (120, 2_228_224, 66_846_720.0),
        (123, 2_228_224, 133_693_440.0),
        (150, 8_912_896, 267_386_880.0),
        (153, 8_912_896, 534_773_760.0),
        (156, 8_912_896, 1_069_547_520.0),
        (180, 35_651_584, 1_069_547_520.0),
        (183, 35_651_584, 2_139_095_040.0),
        (186, 35_651_584, 4_278_190_080.0),
    ];
    for (idc, ps, sr) in LEVELS {
        let max_dpb = if size <= ps >> 2 {
            16
        } else if size <= ps >> 1 {
            12
        } else if size <= (3 * ps) >> 2 {
            8
        } else {
            6
        };
        if idc >= at_least && size <= ps && rate <= sr && dpb <= max_dpb {
            return idc;
        }
    }
    186.max(at_least)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{Decoder, Frame, HwDecode, Packet};
    use crate::info::{read_info, tests::open};
    use fp_ffmpeg_sys as ff;

    fn packet(data: &[u8], pts: i64) -> Packet {
        let pkt = Packet::new();
        // SAFETY: a fresh packet given a buffer of data's size, filled.
        unsafe {
            let p = pkt.as_ptr();
            assert!(ff::av_new_packet(p, data.len() as i32) >= 0);
            std::ptr::copy_nonoverlapping(data.as_ptr(), (*p).data, data.len());
            (*p).pts = pts;
            (*p).dts = pts;
        }
        pkt
    }

    /// A frame's rows, plane by plane.
    fn planes(frame: &Frame) -> Vec<Vec<u8>> {
        // SAFETY: a decoded frame: its planes hold their rows.
        unsafe {
            let f = &*frame.as_ptr();
            let desc = ff::av_pix_fmt_desc_get(f.format);
            (0..ff::av_pix_fmt_count_planes(f.format) as usize)
                .map(|p| {
                    let bytes = ff::av_image_get_linesize(f.format, f.width, p as i32) as usize;
                    let rows = if p == 0 {
                        f.height
                    } else {
                        -((-f.height) >> (*desc).log2_chroma_h)
                    };
                    (0..rows as isize)
                        .flat_map(|y| {
                            std::slice::from_raw_parts(
                                f.data[p].offset(y * f.linesize[p] as isize),
                                bytes,
                            )
                            .to_vec()
                        })
                        .collect()
                })
                .collect()
        }
    }

    /// A copy picture decodes into the very picture it copies: here of a
    /// 10-bit stream.
    #[test]
    fn a_copy_picture_is_its_reference_exactly() {
        let mut input = open("hevc10_tb.mkv");
        let vi = read_info(&input).video_stream().unwrap().index;
        let st = input.streams()[vi];
        // SAFETY: the stream belongs to the open input.
        let (par, time_base) = unsafe { ((*st).codecpar, (*st).time_base) };
        // SAFETY: as above.
        let extradata = crate::mvc::extradata(unsafe { &*par }).to_vec();
        let length_size = usize::from(extradata[21] & 3) + 1;
        let mut first = Vec::new();
        let mut sps = None;
        for nal in super::super::record_units(&extradata) {
            if NalHeader::parse(nal).unwrap().kind == syntax::SPS {
                sps = Some(syntax::sps(&unescape(nal), 0, None).unwrap());
            }
            annex_b(nal, &mut first);
        }
        let sps = sps.unwrap();
        first.extend(copy_pps(sps.id));
        let mut pkt = Packet::new();
        while !(input.read(pkt.as_ptr()).unwrap() && pkt.stream_index() == vi) {
            pkt.unref();
        }
        // SAFETY: a read packet.
        let data = unsafe {
            let p = &*pkt.as_ptr();
            std::slice::from_raw_parts(p.data, p.size as usize)
        };
        for nal in crate::nal::units(data, length_size) {
            assert!(!NalHeader::parse(nal).unwrap().vcl() || NalHeader::parse(nal).unwrap().idr());
            annex_b(nal, &mut first);
        }
        let copy = copy_picture(
            &Shape::of(&sps),
            syntax::TRAIL_R,
            0,
            1,
            &split(&[(-1, true)]).unwrap(),
            true,
        );
        // SAFETY: copies the parameters into a fresh allocation, freed below.
        let mut dec = unsafe {
            let mut p = ff::avcodec_parameters_alloc();
            assert!(ff::avcodec_parameters_copy(p, par) >= 0);
            ff::av_freep((&raw mut (*p).extradata).cast());
            (*p).extradata_size = 0;
            let dec = Decoder::open_with(p, time_base, HwDecode::Off, false).unwrap();
            ff::avcodec_parameters_free(&mut p);
            dec
        };
        for (i, data) in [first, copy].iter().enumerate() {
            assert!(dec.send(packet(data, i as i64).as_ptr()).unwrap());
        }
        dec.send(std::ptr::null()).unwrap();
        let mut frames = Vec::new();
        let mut frame = Frame::new();
        while dec.receive(&mut frame).unwrap() == Some(true) {
            frames.push(planes(&frame));
        }
        assert_eq!(frames.len(), 2);
        for (p, (a, b)) in frames[0].iter().zip(&frames[1]).enumerate() {
            let differ = a.iter().zip(b).filter(|(x, y)| x != y).count();
            assert_eq!(differ, 0, "plane {p}: {differ} bytes differ");
        }
    }

    /// profile_tier_level(1, 0) of Main at level 6.2.
    fn ptl(w: &mut Writer) {
        w.u(8, 1);
        w.u(32, 0x6000_0000);
        w.u(4, 0b1001);
        w.u(32, 0);
        w.u(12, 0);
        w.u(8, 186);
    }

    /// A VPS and an SPS of 8-bit 4:2:0 pictures of this size, in CTBs of
    /// 64 and coding blocks down to 8, Annex B; and the SPS.
    fn sequence(width: u32, height: u32) -> (Vec<u8>, Sps) {
        sequence_of(width, height, [0, 3, 0, 3, 0, 0])
    }

    /// As [`sequence`], the SPS's block sizes `coding`: its
    /// log2_min_luma_coding_block_size_minus3 to
    /// max_transform_hierarchy_depth_intra.
    fn sequence_of(width: u32, height: u32, coding: [u32; 6]) -> (Vec<u8>, Sps) {
        let mut out = Vec::new();
        let mut w = Writer::new();
        w.extend(&[0x40, 0x01]);
        w.u(4, 0);
        w.u(2, 3);
        w.u(6, 0);
        w.u(3, 0);
        w.flag(true);
        w.u(16, 0xffff);
        ptl(&mut w);
        w.flag(true);
        for v in [1, 0, 0] {
            w.ue(v);
        }
        w.u(6, 0);
        w.ue(0);
        w.u(2, 0);
        w.trailing();
        annex_b_rbsp(&w.into_bytes(), &mut out);
        let mut w = Writer::new();
        w.extend(&[0x42, 0x01]);
        w.u(4, 0);
        w.u(3, 0);
        w.flag(true);
        ptl(&mut w);
        for v in [0, 1, width, height] {
            w.ue(v);
        }
        w.flag(false);
        for v in [0, 0, 4] {
            w.ue(v);
        }
        w.flag(true);
        for v in [1, 0, 0].into_iter().chain(coding) {
            w.ue(v);
        }
        w.u(4, 0b0110); // scaling lists off, AMP and SAO on, PCM off
        w.ue(0);
        w.u(5, 0b01100); // long-term off, TMVP and strong smoothing on
        w.trailing();
        let rbsp = w.into_bytes();
        let sps = syntax::sps(&rbsp, 0, None).unwrap();
        annex_b_rbsp(&rbsp, &mut out);
        (out, sps)
    }

    /// Copies of pictures whose edges cut their coding tree blocks, down to
    /// the smallest coding blocks (as Apple Vision Pro's 2200x2200 do), of a
    /// reference the decoder makes up grey for being missing: grey all over,
    /// so every coding unit parsed as the copy codes it.
    #[test]
    fn copy_pictures_cover_pictures_of_any_size() {
        for (width, height) in [(200, 136), (72, 40), (2200, 2200), (64, 64)] {
            let (mut data, sps) = sequence(width, height);
            assert_eq!(
                (sps.tools.rep.width, sps.tools.rep.height, sps.log2_ctb),
                (width, height, 6)
            );
            data.extend(copy_pps(sps.id));
            data.extend(copy_picture(
                &Shape::of(&sps),
                syntax::TRAIL_R,
                0,
                1,
                &split(&[(-1, true)]).unwrap(),
                true,
            ));
            // SAFETY: fresh parameters of our own.
            let mut dec = unsafe {
                let mut p = ff::avcodec_parameters_alloc();
                (*p).codec_type = ff::AVMEDIA_TYPE_VIDEO;
                (*p).codec_id = ff::AV_CODEC_ID_HEVC;
                let tb = ff::AVRational { num: 1, den: 30 };
                let dec = Decoder::open_with(p, tb, HwDecode::Off, false).unwrap();
                ff::avcodec_parameters_free(&mut p);
                dec
            };
            assert!(dec.send(packet(&data, 0).as_ptr()).unwrap());
            dec.send(std::ptr::null()).unwrap();
            let mut frame = Frame::new();
            assert_eq!(
                dec.receive(&mut frame).unwrap(),
                Some(true),
                "{width}x{height}"
            );
            for (p, plane) in planes(&frame).iter().enumerate() {
                let off = plane.iter().filter(|&&v| v != 128).count();
                assert_eq!(off, 0, "{width}x{height} plane {p}: {off} samples not grey");
            }
        }
    }

    /// The IDR pictures starting the rewritten stream decode, whatever the
    /// coding and transform block sizes: grey all over, intra prediction
    /// from nothing without residual, so every coding unit parsed as coded.
    #[test]
    fn starter_pictures_decode() {
        let cases = [
            ([0, 3, 0, 3, 0, 0], [(200, 136), (72, 40), (2200, 2200)]),
            ([1, 2, 0, 3, 1, 1], [(208, 144), (64, 64), (1024, 576)]),
            ([3, 0, 1, 2, 0, 0], [(128, 192), (2240, 2176), (64, 64)]),
            ([0, 1, 0, 1, 0, 2], [(200, 136), (16, 8), (3840, 2160)]),
        ];
        for (coding, sizes) in cases {
            for (width, height) in sizes {
                let (mut data, sps) = sequence_of(width, height, coding);
                data.extend(copy_pps(sps.id));
                data.extend(starter_picture(&Shape::of(&sps), true));
                // SAFETY: fresh parameters of our own.
                let mut dec = unsafe {
                    let mut p = ff::avcodec_parameters_alloc();
                    (*p).codec_type = ff::AVMEDIA_TYPE_VIDEO;
                    (*p).codec_id = ff::AV_CODEC_ID_HEVC;
                    let tb = ff::AVRational { num: 1, den: 30 };
                    let dec = Decoder::open_with(p, tb, HwDecode::Off, false).unwrap();
                    ff::avcodec_parameters_free(&mut p);
                    dec
                };
                let case = format!("{width}x{height} {coding:?}");
                assert!(dec.send(packet(&data, 0).as_ptr()).unwrap(), "{case}");
                dec.send(std::ptr::null()).unwrap();
                let mut frame = Frame::new();
                assert_eq!(dec.receive(&mut frame).unwrap(), Some(true), "{case}");
                for (p, plane) in planes(&frame).iter().enumerate() {
                    let off = plane.iter().filter(|&&v| v != 128).count();
                    assert_eq!(off, 0, "{case} plane {p}: {off} samples not grey");
                }
            }
        }
    }
}
