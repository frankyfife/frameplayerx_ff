//! HEVC syntax as far as the rewrite needs it: NAL unit headers, the
//! parameter sets of both layers and slice segment headers, with where each
//! part lies in its RBSP so it can be copied bit for bit.
//!
//! Multilayer syntax is read as FFmpeg's MV-HEVC decoder reads it (two
//! layers, one view each, the second predicted from the first), which is
//! what the software path plays: anything beyond that is [`Bad::Unsupported`].

use crate::mvc::Bad;
use crate::mvc::bits::Reader;

pub(crate) const RASL_N: u8 = 8;
pub(crate) const RASL_R: u8 = 9;
pub(crate) const TRAIL_R: u8 = 1;
pub(crate) const IDR_W_RADL: u8 = 19;
pub(crate) const IDR_N_LP: u8 = 20;
pub(crate) const VPS: u8 = 32;
pub(crate) const SPS: u8 = 33;
pub(crate) const PPS: u8 = 34;

/// Slice types.
pub(crate) const B: u32 = 0;
pub(crate) const P: u32 = 1;

fn unsupported<T>(what: &str) -> Result<T, Bad> {
    Err(Bad::Unsupported(what.into()))
}

/// The two bytes before every NAL unit's payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NalHeader {
    pub(crate) kind: u8,
    pub(crate) layer: u8,
    /// TemporalId.
    pub(crate) tid: u8,
}

impl NalHeader {
    pub(crate) fn parse(nal: &[u8]) -> Option<NalHeader> {
        let [a, b, ..] = *nal else {
            return None;
        };
        if a & 0x80 != 0 {
            return None;
        }
        Some(NalHeader {
            kind: a >> 1 & 0x3f,
            layer: (a & 1) << 5 | b >> 3,
            tid: (b & 7).checked_sub(1)?,
        })
    }

    pub(crate) fn bytes(self) -> [u8; 2] {
        [
            self.kind << 1 | self.layer >> 5,
            (self.layer & 31) << 3 | (self.tid + 1),
        ]
    }

    /// A slice segment.
    #[cfg(test)]
    pub(crate) fn vcl(self) -> bool {
        self.kind < 32
    }

    pub(crate) fn irap(self) -> bool {
        (16..=23).contains(&self.kind)
    }

    pub(crate) fn idr(self) -> bool {
        matches!(self.kind, IDR_W_RADL | IDR_N_LP)
    }

    pub(crate) fn rasl(self) -> bool {
        matches!(self.kind, RASL_N | RASL_R)
    }

    pub(crate) fn bla(self) -> bool {
        (16..=18).contains(&self.kind)
    }

    /// Not a reference for pictures of its own sub-layer: TemporalId 0
    /// ones of this kind don't anchor picture order counts.
    pub(crate) fn sub_layer_non_reference(self) -> bool {
        self.kind <= 14 && self.kind.is_multiple_of(2)
    }
}

/// A short-term reference picture set: the pictures before the current one
/// (closest first) and after it, as POC differences, and whether the
/// current picture predicts from each.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StRps {
    pub(crate) before: Vec<(i32, bool)>,
    pub(crate) after: Vec<(i32, bool)>,
}

impl StRps {
    pub(crate) fn all(&self) -> impl Iterator<Item = (i32, bool)> + '_ {
        self.before.iter().chain(&self.after).copied()
    }
}

/// `st_ref_pic_set(idx)`, `sets` holding those of the SPS before it (all of
/// them, for one in a slice header).
fn st_rps(r: &mut Reader, idx: usize, sets: &[StRps], in_slice: bool) -> Result<StRps, Bad> {
    if idx != 0 && r.flag()? {
        // inter_ref_pic_set_prediction_flag
        let delta_idx = if in_slice { r.ue()? as usize + 1 } else { 1 };
        let reference = idx
            .checked_sub(delta_idx)
            .and_then(|i| sets.get(i))
            .ok_or(Bad::Broken("RPS predicted from a missing set"))?;
        let sign = r.flag()?;
        let abs = r.ue()? as i32 + 1;
        let delta_rps = if sign { -abs } else { abs };
        let (neg, pos) = (&reference.before, &reference.after);
        let n = neg.len() + pos.len();
        let mut used = vec![false; n + 1];
        let mut use_delta = vec![true; n + 1];
        for j in 0..=n {
            used[j] = r.flag()?;
            if !used[j] {
                use_delta[j] = r.flag()?;
            }
        }
        let mut out = StRps::default();
        for j in (0..pos.len()).rev() {
            let d = pos[j].0 + delta_rps;
            if d < 0 && use_delta[neg.len() + j] {
                out.before.push((d, used[neg.len() + j]));
            }
        }
        if delta_rps < 0 && use_delta[n] {
            out.before.push((delta_rps, used[n]));
        }
        for j in 0..neg.len() {
            let d = neg[j].0 + delta_rps;
            if d < 0 && use_delta[j] {
                out.before.push((d, used[j]));
            }
        }
        for j in (0..neg.len()).rev() {
            let d = neg[j].0 + delta_rps;
            if d > 0 && use_delta[j] {
                out.after.push((d, used[j]));
            }
        }
        if delta_rps > 0 && use_delta[n] {
            out.after.push((delta_rps, used[n]));
        }
        for j in 0..pos.len() {
            let d = pos[j].0 + delta_rps;
            if d > 0 && use_delta[neg.len() + j] {
                out.after.push((d, used[neg.len() + j]));
            }
        }
        return Ok(out);
    }
    let negatives = r.ue()?;
    let positives = r.ue()?;
    if negatives > 16 || positives > 16 {
        return Err(Bad::Broken("RPS too large"));
    }
    let mut out = StRps::default();
    let mut poc = 0;
    for _ in 0..negatives {
        poc -= r.ue()? as i32 + 1;
        out.before.push((poc, r.flag()?));
    }
    poc = 0;
    for _ in 0..positives {
        poc += r.ue()? as i32 + 1;
        out.after.push((poc, r.flag()?));
    }
    Ok(out)
}

/// Skips `profile_tier_level(profile_present, max_sub_layers_minus1)`;
/// returns where its general_level_idc is.
fn ptl(r: &mut Reader, profile_present: bool, max_sub_layers_minus1: u32) -> Result<usize, Bad> {
    if profile_present {
        r.u(8)?; // profile space, tier, profile
        r.u(32)?;
        r.u(32)?; // the source and constraint flags (48 bits)
        r.u(16)?;
    }
    let level = r.pos();
    r.u(8)?;
    let mut present = Vec::new();
    for _ in 0..max_sub_layers_minus1 {
        present.push((r.flag()?, r.flag()?));
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.u(2)?;
        }
    }
    for (profile, level) in present {
        if profile {
            r.u(32)?;
            r.u(32)?;
            r.u(24)?;
        }
        if level {
            r.u(8)?;
        }
    }
    Ok(level)
}

fn hrd(r: &mut Reader, common: bool, max_sub_layers: u32) -> Result<(), Bad> {
    let (mut nal, mut vcl, mut sub_pic) = (false, false, false);
    if common {
        nal = r.flag()?;
        vcl = r.flag()?;
        if nal || vcl {
            sub_pic = r.flag()?;
            if sub_pic {
                r.u(19)?;
            }
            r.u(8)?;
            if sub_pic {
                r.u(4)?;
            }
            r.u(15)?;
        }
    }
    for _ in 0..max_sub_layers {
        let general = r.flag()?;
        let within = if general { false } else { r.flag()? };
        let mut low_delay = false;
        if general || within {
            r.ue()?;
        } else {
            low_delay = r.flag()?;
        }
        let cpbs = if low_delay { 1 } else { r.ue()? + 1 };
        if cpbs > 32 {
            return Err(Bad::Broken("too many CPBs"));
        }
        for _ in 0..u32::from(nal) + u32::from(vcl) {
            for _ in 0..cpbs {
                r.ue()?;
                r.ue()?;
                if sub_pic {
                    r.ue()?;
                    r.ue()?;
                }
                r.flag()?;
            }
        }
    }
    Ok(())
}

/// The picture format of a layer, from its SPS or the VPS extension.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RepFormat {
    pub(crate) chroma_format_idc: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) bit_depth_luma: u32,
    pub(crate) bit_depth_chroma: u32,
}

/// DPB limits for the highest sub-layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Dpb {
    pub(crate) max_dec_pic_buffering: u32,
    pub(crate) num_reorder_pics: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct Vps {
    pub(crate) id: u32,
    pub(crate) max_sub_layers_minus1: u32,
    pub(crate) temporal_id_nesting: bool,
    /// The general profile_tier_level, bits of the RBSP.
    pub(crate) ptl: (usize, usize),
    pub(crate) level_at: usize,
    /// The second layer's nuh_layer_id, format and DPB limits, from the
    /// extension.
    pub(crate) layer1: Option<u8>,
    pub(crate) rep: Option<RepFormat>,
    pub(crate) layer1_dpb: Option<Dpb>,
    pub(crate) default_ref_layers_active: bool,
    /// The views' view_id_val, base layer's first.
    pub(crate) view_ids: [u32; 2],
}

pub(crate) fn vps(rbsp: &[u8]) -> Result<Vps, Bad> {
    let mut r = Reader::new(rbsp, 16);
    let id = r.u(4)?;
    if r.u(2)? != 3 {
        return Err(Bad::Broken("VPS reserved bits"));
    }
    let max_layers = r.u(6)? + 1;
    let max_sub_layers_minus1 = r.u(3)?;
    let temporal_id_nesting = r.flag()?;
    r.u(16)?;
    let start = r.pos();
    let level_at = ptl(&mut r, true, max_sub_layers_minus1)?;
    let ptl_end = r.pos();
    let present = r.flag()?;
    for _ in if present { 0 } else { max_sub_layers_minus1 }..=max_sub_layers_minus1 {
        r.ue()?;
        r.ue()?;
        r.ue()?;
    }
    let max_layer_id = r.u(6)?;
    let layer_sets = r.ue()? + 1;
    if layer_sets > 1024 {
        return Err(Bad::Broken("too many layer sets"));
    }
    let mut layer1_included = 0u64;
    for i in 1..layer_sets {
        for j in 0..=max_layer_id {
            if r.flag()? && i == 1 {
                layer1_included |= 1 << j;
            }
        }
    }
    if r.flag()? {
        // vps_timing_info_present_flag
        r.u(32)?;
        r.u(32)?;
        if r.flag()? {
            r.ue()?;
        }
        let hrds = r.ue()?;
        if hrds > layer_sets {
            return Err(Bad::Broken("too many HRDs"));
        }
        for i in 0..hrds {
            r.ue()?;
            let common = i == 0 || r.flag()?;
            hrd(&mut r, common, max_sub_layers_minus1 + 1)?;
        }
    }
    let mut out = Vps {
        id,
        max_sub_layers_minus1,
        temporal_id_nesting,
        ptl: (start, ptl_end),
        level_at,
        layer1: None,
        rep: None,
        layer1_dpb: None,
        default_ref_layers_active: false,
        view_ids: [0, 1],
    };
    if max_layers > 1 && layer_sets > 1 && r.flag()? {
        extension(&mut r, &mut out, max_layers, layer_sets, layer1_included)?;
    }
    Ok(out)
}

/// The VPS extension, as far as FFmpeg reads it.
fn extension(
    r: &mut Reader,
    vps: &mut Vps,
    max_layers: u32,
    layer_sets: u32,
    layer1_included: u64,
) -> Result<(), Bad> {
    if max_layers > 2 || layer_sets > 2 {
        return unsupported("more than two views");
    }
    while !r.pos().is_multiple_of(8) {
        r.flag()?;
    }
    ptl(r, false, vps.max_sub_layers_minus1)?;
    let splitting = r.flag()?;
    for i in 0..16 {
        if r.flag()? != (i == 1) {
            return unsupported("a scalability other than multiview");
        }
    }
    let dimension_id_len = if splitting { 0 } else { r.u(3)? + 1 };
    let layer1 = if r.flag()? { r.u(6)? as u8 } else { 1 };
    if layer1 == 0 {
        return Err(Bad::Broken("second layer ID 0"));
    }
    if !splitting && r.u(dimension_id_len)? != 1 {
        return unsupported("view order");
    }
    let view_id_len = r.u(4)?;
    if view_id_len > 0 {
        for v in &mut vps.view_ids {
            *v = r.u(view_id_len)?;
        }
    }
    if !r.flag()? {
        return unsupported("views decoded independently");
    }
    let max_present = r.flag()?;
    let mut max_sub = [vps.max_sub_layers_minus1 + 1; 2];
    if max_present {
        for m in &mut max_sub {
            *m = r.u(3)? + 1;
        }
    }
    if r.flag()? {
        r.u(3)?;
    }
    vps.default_ref_layers_active = r.flag()?;
    let ptls = r.ue()? + 1;
    for _ in 2..ptls {
        let present = r.flag()?;
        ptl(r, present, vps.max_sub_layers_minus1)?;
    }
    if r.ue()? != 0 {
        return unsupported("additional output layer sets");
    }
    if r.u(2)? != 0 {
        return unsupported("default_output_layer_idc");
    }
    if layer1_included != (1 | 1 << layer1) {
        return unsupported("layer sets");
    }
    let bits = 32 - (ptls - 1).leading_zeros();
    for _ in 0..2 {
        r.u(if ptls > 1 { bits } else { 0 })?;
    }
    if r.ue()? != 0 {
        return unsupported("several picture formats");
    }
    let mut rep = RepFormat {
        width: r.u(16)?,
        height: r.u(16)?,
        ..Default::default()
    };
    if !r.flag()? {
        return unsupported("picture format");
    }
    rep.chroma_format_idc = r.u(2)?;
    if rep.chroma_format_idc == 3 && r.flag()? {
        return unsupported("separate colour planes");
    }
    rep.bit_depth_luma = r.u(4)? + 8;
    rep.bit_depth_chroma = r.u(4)? + 8;
    if r.flag()? {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    r.flag()?; // max_one_active_ref_layer_flag
    r.flag()?; // vps_poc_lsb_aligned_flag
    let info = r.flag()?;
    let mut dpb = Dpb::default();
    for j in 0..max_sub[0].max(max_sub[1]) {
        if j == 0 || !info || r.flag()? {
            for _ in 0..2 {
                dpb.max_dec_pic_buffering = r.ue()? + 1;
            }
            dpb.num_reorder_pics = r.ue()?;
            r.ue()?;
        }
    }
    vps.layer1 = Some(layer1);
    vps.rep = Some(rep);
    vps.layer1_dpb = Some(dpb);
    Ok(())
}

/// What decides how a layer's slices decode: both layers must agree on all
/// of it to be merged into one stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Tools {
    pub(crate) rep: RepFormat,
    pub(crate) coding: [u32; 6],
    pub(crate) scaling: Option<Vec<u8>>,
    pub(crate) flags: [bool; 4],
    pub(crate) pcm: Option<[u32; 5]>,
}

#[derive(Clone, Debug)]
pub(crate) struct Sps {
    pub(crate) id: u32,
    pub(crate) tools: Tools,
    pub(crate) log2_max_poc_lsb: u32,
    pub(crate) dpb: Dpb,
    pub(crate) st_rps: Vec<StRps>,
    pub(crate) long_term: bool,
    pub(crate) lt_sps: u32,
    pub(crate) temporal_mvp: bool,
    pub(crate) sao: bool,
    pub(crate) log2_ctb: u32,
    pub(crate) log2_min_cb: u32,
    /// For a base layer SPS, the bits around the fields the merged SPS
    /// changes: general_level_idc, log2_max_pic_order_cnt_lsb_minus4 and
    /// the sub-layer ordering info, and the reference picture sets.
    pub(crate) level_at: usize,
    pub(crate) poc_bits: (usize, usize),
    pub(crate) ordering: (usize, usize),
    pub(crate) refs: (usize, usize),
}

impl Sps {
    pub(crate) fn chroma(&self) -> bool {
        self.tools.rep.chroma_format_idc != 0
    }

    pub(crate) fn ctbs(&self) -> (u32, u32) {
        let s = 1 << self.log2_ctb;
        (
            self.tools.rep.width.div_ceil(s),
            self.tools.rep.height.div_ceil(s),
        )
    }
}

fn scaling_list(r: &mut Reader, chroma_format_idc: u32) -> Result<(), Bad> {
    for size in 0..4 {
        let mut matrix = 0;
        while matrix < 6 {
            if !r.flag()? {
                r.ue()?;
            } else {
                let coefs = 64.min(1 << (4 + (size << 1)));
                if size > 1 {
                    r.se()?;
                }
                for _ in 0..coefs {
                    r.se()?;
                }
            }
            matrix += if size == 3 && chroma_format_idc != 3 {
                3
            } else {
                1
            };
        }
    }
    Ok(())
}

/// Reads an SPS of layer `layer`; `vps` gives what a second layer's SPS
/// leaves out.
pub(crate) fn sps(rbsp: &[u8], layer: u8, vps: Option<&Vps>) -> Result<Sps, Bad> {
    let mut r = Reader::new(rbsp, 16);
    r.u(4)?;
    let mut max_sub_minus1 = r.u(3)?;
    let multilayer = layer > 0 && max_sub_minus1 == 7;
    let mut level_at = 0;
    if multilayer {
        max_sub_minus1 = vps
            .ok_or(Bad::Broken("SPS before its VPS"))?
            .max_sub_layers_minus1;
    } else {
        r.flag()?;
        let start = r.pos();
        level_at = ptl(&mut r, true, max_sub_minus1)?;
        let profile = rbsp[start / 8] & 0x1f;
        // Main, Main 10, Main Still Picture, Multiview Main: the profiles
        // whose SPS ends in nothing that changes decoding.
        if !matches!(profile, 1..=3 | 6) {
            return unsupported("this HEVC profile");
        }
    }
    let id = r.ue()?;
    if id > 15 {
        return Err(Bad::Broken("SPS id"));
    }
    let mut tools = Tools::default();
    if multilayer {
        if r.flag()? && r.u(8)? != 0 {
            return unsupported("several picture formats");
        }
        tools.rep = vps
            .and_then(|v| v.rep)
            .ok_or(Bad::Broken("no picture format"))?;
    } else {
        tools.rep.chroma_format_idc = r.ue()?;
        if tools.rep.chroma_format_idc > 3 {
            return Err(Bad::Broken("chroma format"));
        }
        if tools.rep.chroma_format_idc == 3 && r.flag()? {
            return unsupported("separate colour planes");
        }
        tools.rep.width = r.ue()?;
        tools.rep.height = r.ue()?;
        if r.flag()? {
            for _ in 0..4 {
                r.ue()?;
            }
        }
        tools.rep.bit_depth_luma = r.ue()? + 8;
        tools.rep.bit_depth_chroma = r.ue()? + 8;
    }
    let poc_start = r.pos();
    let log2_max_poc_lsb = r.ue()? + 4;
    if log2_max_poc_lsb > 16 {
        return Err(Bad::Broken("POC LSB length"));
    }
    let poc_end = r.pos();
    let mut dpb = vps
        .and_then(|v| if multilayer { v.layer1_dpb } else { None })
        .unwrap_or_default();
    if !multilayer {
        let present = r.flag()?;
        for _ in if present { 0 } else { max_sub_minus1 }..=max_sub_minus1 {
            dpb.max_dec_pic_buffering = r.ue()? + 1;
            dpb.num_reorder_pics = r.ue()?;
            r.ue()?;
        }
    }
    let ordering_end = r.pos();
    let mut coding = [0; 6];
    for c in &mut coding {
        *c = r.ue()?;
    }
    tools.coding = coding;
    let log2_min_cb = coding[0] + 3;
    let log2_ctb = log2_min_cb + coding[1];
    if !(4..=6).contains(&log2_ctb) {
        return Err(Bad::Broken("CTB size"));
    }
    if r.flag()? {
        if multilayer && r.flag()? {
            return unsupported("scaling lists from the other view");
        }
        let start = r.pos();
        if r.flag()? {
            scaling_list(&mut r, tools.rep.chroma_format_idc)?;
        }
        let end = r.pos();
        let mut w = crate::mvc::bits::Writer::new();
        w.copy(rbsp, start, end);
        w.align_ones();
        tools.scaling = Some(w.into_bytes());
    }
    let amp = r.flag()?;
    let sao = r.flag()?;
    if r.flag()? {
        let mut pcm = [0; 5];
        pcm[0] = r.u(4)?;
        pcm[1] = r.u(4)?;
        pcm[2] = r.ue()?;
        pcm[3] = r.ue()?;
        pcm[4] = u32::from(r.flag()?);
        tools.pcm = Some(pcm);
    }
    let refs_start = r.pos();
    let sets = r.ue()? as usize;
    if sets > 64 {
        return Err(Bad::Broken("too many RPSs"));
    }
    let mut st = Vec::with_capacity(sets);
    for i in 0..sets {
        let s = st_rps(&mut r, i, &st, false)?;
        st.push(s);
    }
    let long_term = r.flag()?;
    let mut lt_sps = 0;
    if long_term {
        lt_sps = r.ue()?;
        if lt_sps > 32 {
            return Err(Bad::Broken("too many long-term pictures"));
        }
        for _ in 0..lt_sps {
            r.u(log2_max_poc_lsb)?;
            r.flag()?;
        }
    }
    let refs_end = r.pos();
    let temporal_mvp = r.flag()?;
    let strong_intra = r.flag()?;
    tools.flags = [amp, sao, temporal_mvp, strong_intra];
    Ok(Sps {
        id,
        tools,
        log2_max_poc_lsb,
        dpb,
        st_rps: st,
        long_term,
        lt_sps,
        temporal_mvp,
        sao,
        log2_ctb,
        log2_min_cb,
        level_at,
        poc_bits: (poc_start, poc_end),
        ordering: (poc_end, ordering_end),
        refs: (refs_start, refs_end),
    })
}

#[derive(Clone, Debug)]
pub(crate) struct Pps {
    pub(crate) id: u32,
    pub(crate) sps_id: u32,
    pub(crate) dependent_slices: bool,
    pub(crate) output_flag_present: bool,
    pub(crate) extra_slice_header_bits: u32,
    pub(crate) cabac_init_present: bool,
    pub(crate) num_ref_idx_default: [u32; 2],
    pub(crate) slice_chroma_qp_offsets: bool,
    pub(crate) weighted_pred: bool,
    pub(crate) weighted_bipred: bool,
    pub(crate) entry_points: bool,
    pub(crate) loop_filter_across_slices: bool,
    pub(crate) deblocking_override: bool,
    pub(crate) deblocking_disabled: bool,
    pub(crate) lists_modification: bool,
    pub(crate) slice_header_extension: bool,
    pub(crate) chroma_qp_offset_list: bool,
    /// Where the IDs end; where lists_modification_present_flag is; where
    /// slice_segment_header_extension_present_flag is.
    pub(crate) after_ids: usize,
    pub(crate) lists_at: usize,
    pub(crate) extension_at: usize,
    /// The range extension, when there is one.
    pub(crate) range: Option<(usize, usize)>,
}

/// Reads a PPS; `sps` finds the SPS it names.
pub(crate) fn pps<'a>(rbsp: &[u8], sps: impl Fn(u32) -> Option<&'a Sps>) -> Result<Pps, Bad> {
    let mut r = Reader::new(rbsp, 16);
    let id = r.ue()?;
    let sps_id = r.ue()?;
    if id > 63 || sps_id > 15 {
        return Err(Bad::Broken("PPS id"));
    }
    let s = sps(sps_id).ok_or(Bad::Broken("PPS before its SPS"))?;
    let after_ids = r.pos();
    let dependent_slices = r.flag()?;
    let output_flag_present = r.flag()?;
    let extra_slice_header_bits = r.u(3)?;
    r.flag()?; // sign_data_hiding_enabled_flag
    let cabac_init_present = r.flag()?;
    let num_ref_idx_default = [r.ue()? + 1, r.ue()? + 1];
    r.se()?;
    r.flag()?;
    let transform_skip = r.flag()?;
    if r.flag()? {
        r.ue()?;
    }
    r.se()?;
    r.se()?;
    let slice_chroma_qp_offsets = r.flag()?;
    let weighted_pred = r.flag()?;
    let weighted_bipred = r.flag()?;
    r.flag()?; // transquant_bypass_enabled_flag
    let tiles = r.flag()?;
    let wpp = r.flag()?;
    if tiles {
        let columns = r.ue()?;
        let rows = r.ue()?;
        if columns > 64 || rows > 64 {
            return Err(Bad::Broken("too many tiles"));
        }
        if !r.flag()? {
            for _ in 0..columns + rows {
                r.ue()?;
            }
        }
        r.flag()?;
    }
    let loop_filter_across_slices = r.flag()?;
    let (mut deblocking_override, mut deblocking_disabled) = (false, false);
    if r.flag()? {
        deblocking_override = r.flag()?;
        deblocking_disabled = r.flag()?;
        if !deblocking_disabled {
            r.se()?;
            r.se()?;
        }
    }
    if r.flag()? {
        scaling_list(&mut r, s.tools.rep.chroma_format_idc)?;
    }
    let lists_at = r.pos();
    let lists_modification = r.flag()?;
    r.ue()?;
    let extension_at = r.pos();
    let slice_header_extension = r.flag()?;
    let mut range = None;
    let mut chroma_qp_offset_list = false;
    if r.flag()? {
        let range_present = r.flag()?;
        let multilayer = r.flag()?;
        let three_d = r.flag()?;
        let scc = r.flag()?;
        r.u(4)?;
        if three_d || scc {
            return unsupported("3D or screen content extensions");
        }
        if range_present {
            let start = r.pos();
            if transform_skip {
                r.ue()?;
            }
            r.flag()?;
            chroma_qp_offset_list = r.flag()?;
            if chroma_qp_offset_list {
                r.ue()?;
                let n = r.ue()? + 1;
                if n > 6 {
                    return Err(Bad::Broken("chroma QP offset list"));
                }
                for _ in 0..n {
                    r.se()?;
                    r.se()?;
                }
            }
            r.ue()?;
            r.ue()?;
            range = Some((start, r.pos()));
        }
        if multilayer {
            // Its scaling list inference and colour mapping change how the
            // view decodes; the rest is for spatial scalability.
            r.flag()?;
            if r.flag()? {
                return unsupported("scaling lists from the other view");
            }
            let offsets = r.ue()?;
            if offsets > 1 {
                return Err(Bad::Broken("reference location offsets"));
            }
            for _ in 0..offsets {
                r.u(6)?;
                for count in [4, 4] {
                    if r.flag()? {
                        for _ in 0..count {
                            r.se()?;
                        }
                    }
                }
                if r.flag()? {
                    for _ in 0..4 {
                        r.ue()?;
                    }
                }
            }
            if r.flag()? {
                return unsupported("colour mapping");
            }
        }
    }
    Ok(Pps {
        id,
        sps_id,
        dependent_slices,
        output_flag_present,
        extra_slice_header_bits,
        cabac_init_present,
        num_ref_idx_default,
        slice_chroma_qp_offsets,
        weighted_pred,
        weighted_bipred,
        entry_points: tiles || wpp,
        loop_filter_across_slices,
        deblocking_override,
        deblocking_disabled,
        lists_modification,
        slice_header_extension,
        chroma_qp_offset_list,
        after_ids,
        lists_at,
        extension_at,
        range,
    })
}

/// A slice segment header: its values, and where its parts lie.
#[derive(Clone, Debug, Default)]
pub(crate) struct Slice {
    pub(crate) first: bool,
    pub(crate) pps_id: u32,
    pub(crate) dependent: bool,
    /// Bits of the RBSP: from the end of slice_pic_parameter_set_id to the
    /// end of slice_segment_address (dependent_slice_segment_flag and the
    /// address); the slice_reserved_flags.
    pub(crate) address: (usize, usize),
    pub(crate) reserved: (usize, usize),
    pub(crate) slice_type: u32,
    pub(crate) pic_output: Option<bool>,
    /// Where slice_pic_order_cnt_lsb is (or would be).
    pub(crate) poc_at: usize,
    pub(crate) poc_lsb: Option<u32>,
    /// None for an IDR picture.
    pub(crate) st_rps: Option<StRps>,
    pub(crate) long_term_pictures: u32,
    pub(crate) temporal_mvp: bool,
    pub(crate) inter_layer: bool,
    /// From slice_temporal_mvp_enabled_flag (or where it would be) to the
    /// SAO flags' end: what of a base slice follows its references.
    pub(crate) after_refs: usize,
    pub(crate) sao: (usize, usize),
    pub(crate) num_ref_idx: [u32; 2],
    pub(crate) list_entry: [Option<Vec<u32>>; 2],
    pub(crate) collocated: Option<(usize, u32)>,
    /// From mvd_l1_zero_flag (for an I slice, slice_qp_delta) to the end
    /// of the loop filter flags; the entry points; the header extension.
    pub(crate) tail: (usize, usize),
    pub(crate) entry: (usize, usize),
    pub(crate) extension: (usize, usize),
    /// Where the slice data begins, in bytes of the RBSP.
    pub(crate) data: usize,
}

/// Reads a slice segment header. `layer1` tells the second layer's
/// inter-layer syntax apart; `previous` is the independent segment a
/// dependent one belongs to.
pub(crate) fn slice(
    rbsp: &[u8],
    nal: NalHeader,
    pps: &Pps,
    sps: &Sps,
    vps: Option<&Vps>,
    previous: Option<&Slice>,
) -> Result<Slice, Bad> {
    let mut r = Reader::new(rbsp, 16);
    let mut s = Slice {
        first: r.flag()?,
        ..Default::default()
    };
    if nal.irap() {
        r.flag()?;
    }
    s.pps_id = r.ue()?;
    let address_start = r.pos();
    if !s.first {
        if pps.dependent_slices {
            s.dependent = r.flag()?;
        }
        let (w, h) = sps.ctbs();
        let n = w * h;
        r.u(32 - (n - 1).leading_zeros())?;
    }
    s.address = (address_start, r.pos());
    if s.dependent {
        let p = previous.ok_or(Bad::Broken("dependent slice segment first"))?;
        s.slice_type = p.slice_type;
        s.poc_lsb = p.poc_lsb;
        s.st_rps = p.st_rps.clone();
        s.inter_layer = p.inter_layer;
        s.num_ref_idx = p.num_ref_idx;
        s.list_entry = p.list_entry.clone();
        s.collocated = p.collocated;
    } else {
        let reserved = r.pos();
        r.u(pps.extra_slice_header_bits)?;
        s.reserved = (reserved, r.pos());
        s.slice_type = r.ue()?;
        if s.slice_type > 2 {
            return Err(Bad::Broken("slice type"));
        }
        if pps.output_flag_present {
            s.pic_output = Some(r.flag()?);
        }
        s.poc_at = r.pos();
        if !nal.idr() || nal.layer > 0 {
            s.poc_lsb = Some(r.u(sps.log2_max_poc_lsb)?);
        }
        if !nal.idr() {
            let in_sps = r.flag()?;
            let rps = if in_sps {
                let n = sps.st_rps.len() as u32;
                if n == 0 {
                    return Err(Bad::Broken("no RPS in the SPS"));
                }
                let idx = r.u(32 - (n - 1).leading_zeros().min(32))?;
                sps.st_rps
                    .get(idx as usize)
                    .cloned()
                    .ok_or(Bad::Broken("RPS index"))?
            } else {
                st_rps(&mut r, sps.st_rps.len(), &sps.st_rps, true)?
            };
            s.st_rps = Some(rps);
            if sps.long_term {
                let mut n = 0;
                if sps.lt_sps > 0 {
                    n = r.ue()?;
                }
                n += r.ue()?;
                s.long_term_pictures = n;
                if n > 0 {
                    return unsupported("long-term references");
                }
            }
        }
        s.after_refs = r.pos();
        if !nal.idr() && sps.temporal_mvp {
            s.temporal_mvp = r.flag()?;
        }
        if nal.layer > 0 {
            let v = vps.ok_or(Bad::Broken("no VPS"))?;
            s.inter_layer = v.default_ref_layers_active || r.flag()?;
        }
        let sao = r.pos();
        if sps.sao {
            r.flag()?;
            if sps.chroma() {
                r.flag()?;
            }
        }
        s.sao = (sao, r.pos());
        let mut tail = r.pos();
        if s.slice_type != 2 {
            let lists = if s.slice_type == B { 2 } else { 1 };
            s.num_ref_idx = pps.num_ref_idx_default;
            if s.slice_type != B {
                s.num_ref_idx[1] = 0;
            }
            if r.flag()? {
                for l in 0..lists {
                    s.num_ref_idx[l] = r.ue()? + 1;
                }
            }
            if s.num_ref_idx.iter().any(|&n| n > 15) {
                return Err(Bad::Broken("too many references"));
            }
            let rps = s.st_rps.as_ref();
            let total = rps.map_or(0, |p| p.all().filter(|e| e.1).count() as u32)
                + u32::from(s.inter_layer);
            if total == 0 {
                return Err(Bad::Broken("an inter slice without references"));
            }
            if pps.lists_modification && total > 1 {
                let bits = 32 - (total - 1).leading_zeros();
                for l in 0..lists {
                    if r.flag()? {
                        let mut e = Vec::new();
                        for _ in 0..s.num_ref_idx[l] {
                            e.push(r.u(bits)?);
                        }
                        s.list_entry[l] = Some(e);
                    }
                }
            }
            tail = r.pos();
            if s.slice_type == B {
                r.flag()?;
            }
            if pps.cabac_init_present {
                r.flag()?;
            }
            if s.temporal_mvp {
                let list = if s.slice_type == B && !r.flag()? {
                    1
                } else {
                    0
                };
                let idx = if s.num_ref_idx[list] > 1 { r.ue()? } else { 0 };
                s.collocated = Some((list, idx));
            }
            if (pps.weighted_pred && s.slice_type == P)
                || (pps.weighted_bipred && s.slice_type == B)
            {
                pred_weight_table(&mut r, &s, sps.chroma())?;
            }
            r.ue()?;
        }
        r.se()?;
        if pps.slice_chroma_qp_offsets {
            r.se()?;
            r.se()?;
        }
        if pps.chroma_qp_offset_list {
            r.flag()?;
        }
        let mut deblocking_disabled = pps.deblocking_disabled;
        if pps.deblocking_override && r.flag()? {
            deblocking_disabled = r.flag()?;
            if !deblocking_disabled {
                r.se()?;
                r.se()?;
            }
        }
        let sao_on = rbsp_any(rbsp, s.sao);
        if pps.loop_filter_across_slices && (sao_on || !deblocking_disabled) {
            r.flag()?;
        }
        s.tail = (tail, r.pos());
    }
    let entry = r.pos();
    if pps.entry_points {
        let n = r.ue()?;
        if n > 4096 {
            return Err(Bad::Broken("too many entry points"));
        }
        if n > 0 {
            let len = r.ue()? + 1;
            if len > 32 {
                return Err(Bad::Broken("entry point length"));
            }
            for _ in 0..n {
                r.u(len)?;
            }
        }
    }
    s.entry = (entry, r.pos());
    let extension = r.pos();
    if pps.slice_header_extension {
        let n = r.ue()?;
        for _ in 0..n {
            r.u(8)?;
        }
    }
    s.extension = (extension, r.pos());
    if !r.flag()? {
        return Err(Bad::Broken("slice header alignment"));
    }
    while !r.pos().is_multiple_of(8) {
        if r.flag()? {
            return Err(Bad::Broken("slice header alignment"));
        }
    }
    s.data = r.pos() / 8;
    Ok(s)
}

/// Whether any of the bits `range` of `rbsp` is set.
fn rbsp_any(rbsp: &[u8], range: (usize, usize)) -> bool {
    (range.0..range.1).any(|i| rbsp[i / 8] >> (7 - i % 8) & 1 == 1)
}

fn pred_weight_table(r: &mut Reader, s: &Slice, chroma: bool) -> Result<(), Bad> {
    r.ue()?;
    if chroma {
        r.se()?;
    }
    let lists = if s.slice_type == B { 2 } else { 1 };
    for l in 0..lists {
        let n = s.num_ref_idx[l] as usize;
        let mut luma = vec![false; n];
        let mut chroma_flags = vec![false; n];
        for f in &mut luma {
            *f = r.flag()?;
        }
        if chroma {
            for f in &mut chroma_flags {
                *f = r.flag()?;
            }
        }
        for i in 0..n {
            if luma[i] {
                r.se()?;
                r.se()?;
            }
            if chroma_flags[i] {
                for _ in 0..4 {
                    r.se()?;
                }
            }
        }
    }
    Ok(())
}

/// The POC a picture's LSBs give after `prev` (that of the previous
/// TemporalId 0 picture), with `max` LSB values.
pub(crate) fn poc(prev: i32, lsb: u32, max: u32, bla: bool) -> i32 {
    let max = max as i32;
    let lsb = lsb as i32;
    let prev_lsb = prev.rem_euclid(max);
    let prev_msb = prev - prev_lsb;
    let msb = if bla {
        0
    } else if lsb < prev_lsb && prev_lsb - lsb >= max / 2 {
        prev_msb + max
    } else if lsb > prev_lsb && lsb - prev_lsb > max / 2 {
        prev_msb - max
    } else {
        prev_msb
    };
    msb + lsb
}

/// The views a three-dimensional reference displays information SEI names
/// the left and the right eye's (its first display's), from a prefix SEI
/// NAL unit's RBSP.
pub(crate) fn eye_views(rbsp: &[u8]) -> Option<(u32, u32)> {
    let mut at = 2;
    let end = crate::mvc::bits::stop_bit(rbsp)? / 8;
    while at < end {
        let mut kind = 0usize;
        while *rbsp.get(at)? == 0xff {
            kind += 255;
            at += 1;
        }
        kind += usize::from(rbsp[at]);
        at += 1;
        let mut size = 0usize;
        while *rbsp.get(at)? == 0xff {
            size += 255;
            at += 1;
        }
        size += usize::from(rbsp[at]);
        at += 1;
        let payload = rbsp.get(at..at + size)?;
        if kind == 176 {
            let mut r = Reader::new(payload, 0);
            r.ue().ok()?;
            if r.flag().ok()? {
                r.ue().ok()?;
            }
            r.ue().ok()?;
            return Some((r.ue().ok()?, r.ue().ok()?));
        }
        at += size;
    }
    None
}
