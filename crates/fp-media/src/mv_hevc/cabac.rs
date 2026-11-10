//! A CABAC encoder just big enough for two pictures: one copied whole from
//! its reference (every coding unit skipped, merged with a zero motion
//! vector), and an intra picture without residual that starts a stream.

use crate::mvc::bits::Writer;

/// rangeTabLps, by pStateIdx and qRangeIdx.
#[rustfmt::skip]
const RANGE_LPS: [[u8; 4]; 64] = [
    [128, 176, 208, 240], [128, 167, 197, 227], [128, 158, 187, 216], [123, 150, 178, 205],
    [116, 142, 169, 195], [111, 135, 160, 185], [105, 128, 152, 175], [100, 122, 144, 166],
    [95, 116, 137, 158], [90, 110, 130, 150], [85, 104, 123, 142], [81, 99, 117, 135],
    [77, 94, 111, 128], [73, 89, 105, 122], [69, 85, 100, 116], [66, 80, 95, 110],
    [62, 76, 90, 104], [59, 72, 86, 99], [56, 69, 81, 94], [53, 65, 77, 89],
    [51, 62, 73, 85], [48, 59, 69, 80], [46, 56, 66, 76], [43, 53, 63, 72],
    [41, 50, 59, 69], [39, 48, 56, 65], [37, 45, 54, 62], [35, 43, 51, 59],
    [33, 41, 48, 56], [32, 39, 46, 53], [30, 37, 43, 50], [29, 35, 41, 48],
    [27, 33, 39, 45], [26, 31, 37, 43], [24, 30, 35, 41], [23, 28, 33, 39],
    [22, 27, 32, 37], [21, 26, 30, 35], [20, 24, 29, 33], [19, 23, 27, 31],
    [18, 22, 26, 30], [17, 21, 25, 28], [16, 20, 23, 27], [15, 19, 22, 25],
    [14, 18, 21, 24], [14, 17, 20, 23], [13, 16, 19, 22], [12, 15, 18, 21],
    [12, 14, 17, 20], [11, 14, 16, 19], [11, 13, 15, 18], [10, 12, 15, 17],
    [10, 12, 14, 16], [9, 11, 13, 15], [9, 11, 12, 14], [8, 10, 12, 14],
    [8, 9, 11, 13], [7, 9, 11, 12], [7, 9, 10, 12], [7, 8, 10, 11],
    [6, 8, 9, 11], [6, 7, 9, 10], [6, 7, 8, 9], [2, 2, 2, 2],
];

/// transIdxLps.
#[rustfmt::skip]
const NEXT_LPS: [u8; 64] = [
    0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12,
    13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21, 21, 22, 22, 23, 24,
    24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33,
    33, 33, 34, 34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

/// A context variable: pStateIdx and valMps.
#[derive(Clone, Copy, Debug)]
struct Context {
    state: u8,
    mps: bool,
}

impl Context {
    /// Initialized from initValue for SliceQpY `qp`.
    fn new(init: u8, qp: i32) -> Context {
        let m = i32::from(init >> 4) * 5 - 45;
        let n = (i32::from(init & 15) << 3) - 16;
        let pre = (((m * qp.clamp(0, 51)) >> 4) + n).clamp(1, 126);
        if pre <= 63 {
            Context {
                state: (63 - pre) as u8,
                mps: false,
            }
        } else {
            Context {
                state: (pre - 64) as u8,
                mps: true,
            }
        }
    }
}

struct Encoder {
    out: Writer,
    low: u32,
    range: u32,
    outstanding: u32,
    first: bool,
}

impl Encoder {
    fn new() -> Encoder {
        Encoder {
            out: Writer::new(),
            low: 0,
            range: 510,
            outstanding: 0,
            first: true,
        }
    }

    fn put(&mut self, bit: bool) {
        if self.first {
            self.first = false;
        } else {
            self.out.flag(bit);
        }
        while self.outstanding > 0 {
            self.out.flag(!bit);
            self.outstanding -= 1;
        }
    }

    fn renormalize(&mut self) {
        while self.range < 256 {
            if self.low < 256 {
                self.put(false);
            } else if self.low >= 512 {
                self.low -= 512;
                self.put(true);
            } else {
                self.low -= 256;
                self.outstanding += 1;
            }
            self.range <<= 1;
            self.low <<= 1;
        }
    }

    fn decision(&mut self, ctx: &mut Context, bin: bool) {
        let q = (self.range >> 6 & 3) as usize;
        let lps = u32::from(RANGE_LPS[usize::from(ctx.state)][q]);
        self.range -= lps;
        if bin != ctx.mps {
            self.low += self.range;
            self.range = lps;
            if ctx.state == 0 {
                ctx.mps = !ctx.mps;
            }
            ctx.state = NEXT_LPS[usize::from(ctx.state)];
        } else {
            ctx.state = (ctx.state + 1).min(62);
        }
        self.renormalize();
    }

    /// A bin of equal probabilities.
    fn bypass(&mut self, bin: bool) {
        self.low <<= 1;
        if bin {
            self.low += self.range;
        }
        if self.low >= 1024 {
            self.put(true);
            self.low -= 1024;
        } else if self.low < 512 {
            self.put(false);
        } else {
            self.low -= 512;
            self.outstanding += 1;
        }
    }

    /// A terminating bin. The last (1) ends the slice data: the final bit
    /// it writes is the rbsp_stop_one_bit, and zeros align it.
    fn terminate(&mut self, bin: bool) {
        self.range -= 2;
        if bin {
            self.low += self.range;
            self.range = 2;
            self.renormalize();
            self.put(self.low >> 9 & 1 == 1);
            self.out.u(2, (self.low >> 7 & 3) | 1);
            while !self.out.len().is_multiple_of(8) {
                self.out.flag(false);
            }
        } else {
            self.renormalize();
        }
    }
}

/// The picture being coded: each minimum coding block's quadtree depth.
struct Picture {
    width: u32,
    height: u32,
    log2_min_cb: u32,
    cols: usize,
    depth: Vec<u8>,
}

impl Picture {
    fn at(&self, x: u32, y: u32) -> u8 {
        let m = self.log2_min_cb;
        self.depth[(y >> m) as usize * self.cols + (x >> m) as usize]
    }
}

struct Contexts {
    split: [Context; 3],
    skip: [Context; 3],
}

/// coding_quadtree(): coding units as large as fit, each skipped.
fn quadtree(
    enc: &mut Encoder,
    ctx: &mut Contexts,
    pic: &mut Picture,
    at: (u32, u32),
    log2: u32,
    d: u8,
) {
    let (x0, y0) = at;
    let size = 1 << log2;
    let inside = x0 + size <= pic.width && y0 + size <= pic.height;
    if log2 > pic.log2_min_cb {
        if inside {
            let inc = usize::from(x0 > 0 && pic.at(x0 - 1, y0) > d)
                + usize::from(y0 > 0 && pic.at(x0, y0 - 1) > d);
            enc.decision(&mut ctx.split[inc], false);
        } else {
            // Past the picture's edge: split without saying so.
            let half = size / 2;
            for (dx, dy) in [(0, 0), (half, 0), (0, half), (half, half)] {
                let (x, y) = (x0 + dx, y0 + dy);
                if x < pic.width && y < pic.height {
                    quadtree(enc, ctx, pic, (x, y), log2 - 1, d + 1);
                }
            }
            return;
        }
    }
    let m = pic.log2_min_cb;
    for y in (y0..(y0 + size).min(pic.height)).step_by(1 << m) {
        for x in (x0..(x0 + size).min(pic.width)).step_by(1 << m) {
            pic.depth[(y >> m) as usize * pic.cols + (x >> m) as usize] = d;
        }
    }
    // cu_skip_flag: every neighbour is skipped too.
    let inc = usize::from(x0 > 0) + usize::from(y0 > 0);
    enc.decision(&mut ctx.skip[inc], true);
}

/// The slice data of a P slice, its picture's only one, that copies its
/// first reference whole: every coding unit as large as it can be, skipped
/// (merged, with one merge candidate: the zero motion vector), with SliceQpY
/// 26 and cabac_init_flag 0. `width` and `height` in luma samples.
pub(crate) fn copy_slice_data(width: u32, height: u32, log2_ctb: u32, log2_min_cb: u32) -> Vec<u8> {
    // initValues for initType 1.
    let mut ctx = Contexts {
        split: [107, 139, 126].map(|v| Context::new(v, 26)),
        skip: [197, 185, 201].map(|v| Context::new(v, 26)),
    };
    let mut enc = Encoder::new();
    let min = 1u32 << log2_min_cb;
    let cols = width.div_ceil(min) as usize;
    let mut pic = Picture {
        width,
        height,
        log2_min_cb,
        cols,
        depth: vec![0; cols * height.div_ceil(min) as usize],
    };
    let ctb = 1u32 << log2_ctb;
    let (ctbs_x, ctbs_y) = (width.div_ceil(ctb), height.div_ceil(ctb));
    for cy in 0..ctbs_y {
        for cx in 0..ctbs_x {
            quadtree(
                &mut enc,
                &mut ctx,
                &mut pic,
                (cx * ctb, cy * ctb),
                log2_ctb,
                0,
            );
            enc.terminate(cx + 1 == ctbs_x && cy + 1 == ctbs_y);
        }
    }
    enc.out.into_bytes()
}

/// What the coding of an intra picture depends on: its size, its blocks',
/// and whether it has chroma (4:2:0).
pub(crate) struct Blocks {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) log2_ctb: u32,
    pub(crate) log2_min_cb: u32,
    pub(crate) log2_min_tb: u32,
    pub(crate) log2_max_tb: u32,
    pub(crate) max_depth_intra: u32,
    pub(crate) chroma: bool,
}

struct IntraContexts {
    split: [Context; 3],
    part_mode: Context,
    prev_intra_luma: Context,
    chroma_mode: Context,
    split_transform: [Context; 3],
    cbf_luma: [Context; 2],
    cbf_chroma: [Context; 5],
}

/// The slice data of an I slice, its picture's only one: coding units as
/// small as they go, each predicted in four parts from the most probable
/// mode, without residual, with SliceQpY 26. What it decodes into is no
/// matter (a picture nothing refers to, not shown), only that it decodes.
pub(crate) fn intra_slice_data(b: &Blocks) -> Vec<u8> {
    // initValues for initType 0.
    let mut ctx = IntraContexts {
        split: [139, 141, 157].map(|v| Context::new(v, 26)),
        part_mode: Context::new(184, 26),
        prev_intra_luma: Context::new(184, 26),
        chroma_mode: Context::new(63, 26),
        split_transform: [153, 138, 138].map(|v| Context::new(v, 26)),
        cbf_luma: [111, 141].map(|v| Context::new(v, 26)),
        cbf_chroma: [94, 138, 182, 154, 154].map(|v| Context::new(v, 26)),
    };
    let mut enc = Encoder::new();
    let min = 1u32 << b.log2_min_cb;
    let cols = b.width.div_ceil(min) as usize;
    let mut pic = Picture {
        width: b.width,
        height: b.height,
        log2_min_cb: b.log2_min_cb,
        cols,
        depth: vec![0; cols * b.height.div_ceil(min) as usize],
    };
    let ctb = 1u32 << b.log2_ctb;
    let (ctbs_x, ctbs_y) = (b.width.div_ceil(ctb), b.height.div_ceil(ctb));
    for cy in 0..ctbs_y {
        for cx in 0..ctbs_x {
            intra_quadtree(
                &mut enc,
                &mut ctx,
                b,
                &mut pic,
                (cx * ctb, cy * ctb),
                b.log2_ctb,
                0,
            );
            enc.terminate(cx + 1 == ctbs_x && cy + 1 == ctbs_y);
        }
    }
    enc.out.into_bytes()
}

/// coding_quadtree() split down to the smallest coding units, each an
/// intra one of four prediction units (NxN, which the smallest always may
/// be: transform blocks are smaller than coding blocks).
fn intra_quadtree(
    enc: &mut Encoder,
    ctx: &mut IntraContexts,
    b: &Blocks,
    pic: &mut Picture,
    at: (u32, u32),
    log2: u32,
    d: u8,
) {
    let (x0, y0) = at;
    let size = 1 << log2;
    if log2 > pic.log2_min_cb {
        if x0 + size <= pic.width && y0 + size <= pic.height {
            let inc = usize::from(x0 > 0 && pic.at(x0 - 1, y0) > d)
                + usize::from(y0 > 0 && pic.at(x0, y0 - 1) > d);
            enc.decision(&mut ctx.split[inc], true);
        }
        let half = size / 2;
        for (dx, dy) in [(0, 0), (half, 0), (0, half), (half, half)] {
            let (x, y) = (x0 + dx, y0 + dy);
            if x < pic.width && y < pic.height {
                intra_quadtree(enc, ctx, b, pic, (x, y), log2 - 1, d + 1);
            }
        }
        return;
    }
    let m = pic.log2_min_cb;
    pic.depth[(y0 >> m) as usize * pic.cols + (x0 >> m) as usize] = d;
    // part_mode: NxN.
    enc.decision(&mut ctx.part_mode, false);
    // Each part's luma mode its first most probable one (mpm_idx 0).
    for _ in 0..4 {
        enc.decision(&mut ctx.prev_intra_luma, true);
    }
    for _ in 0..4 {
        enc.bypass(false);
    }
    if b.chroma {
        // intra_chroma_pred_mode 4: as luma.
        enc.decision(&mut ctx.chroma_mode, false);
    }
    transform_tree(enc, ctx, b, log2, 0);
}

/// transform_tree() without residual: every coded block flag 0.
fn transform_tree(enc: &mut Encoder, ctx: &mut IntraContexts, b: &Blocks, log2: u32, depth: u32) {
    // NxN splits the coding unit's tree at its root.
    let intra_split = depth == 0;
    let split = if log2 <= b.log2_max_tb
        && log2 > b.log2_min_tb
        && depth < b.max_depth_intra + 1
        && !intra_split
    {
        enc.decision(&mut ctx.split_transform[(5 - log2) as usize], false);
        false
    } else {
        log2 > b.log2_max_tb || intra_split
    };
    // cbf_cb and cbf_cr: below the root only under a set one.
    if b.chroma && log2 > 2 && depth == 0 {
        enc.decision(&mut ctx.cbf_chroma[0], false);
        enc.decision(&mut ctx.cbf_chroma[0], false);
    }
    if split {
        for _ in 0..4 {
            transform_tree(enc, ctx, b, log2 - 1, depth + 1);
        }
    } else {
        let inc = usize::from(depth == 0);
        enc.decision(&mut ctx.cbf_luma[inc], false);
    }
}
