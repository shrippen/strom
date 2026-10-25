//! Sink pad indices of the vision mixer's compositors.
//!
//! Every compositor pad the vision mixer requests has a fixed index derived
//! from the pad *capacity* (the number of input slots the mixer reserves
//! indices for), the number of DSK inputs and the number of PiP tiles. This
//! type is the single place those formulas live; builders and runtime
//! operations ask it instead of doing index arithmetic themselves.
//!
//! Distribution compositor (`mixer`):
//! ```text
//! sink_0 .. sink_{C-1}            inputs
//! sink_C .. sink_{C+D-1}          DSK layers
//! sink_{C+D} .. sink_{2C+D-1}     border underlays (only with PiPs)
//! sink_{stinger}, +1              stinger graphic, matte (GPU only)
//! ```
//! Multiview compositor (`mv_comp`):
//! ```text
//! sink_0 .. sink_{C-1}            thumbnails
//! sink_C                          PGM big display
//! sink_{C+1} .. sink_{2C}         PVW big candidates
//! sink_{2C+1+p*C+i}               PiP tile p, input i
//! sink_{2C+1+P*C}                 cairo overlay
//! sink_{overlay+1+i}              PVW border underlays (only with PiPs)
//! sink_{overlay+1+C*(1+p)+i}      PiP tile border underlays (only with PiPs)
//! ```
//! The dist pad of input `i` is `sink_i` on purpose: transitions, the T-bar
//! and the stinger address it by input index (an invariant of this map).
//!
//! Preview output compositor (`pvw_comp`): inputs at `sink_i`, border
//! underlays (only with PiPs) at `sink_{C+i}`.

/// Sink pad indices of the vision mixer's compositors (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadMap {
    /// Number of input slots that pad indices are reserved for.
    pub capacity: usize,
    /// Number of DSK inputs.
    pub num_dsk: usize,
    /// Number of PiP tiles.
    pub num_pips: usize,
}

impl PadMap {
    pub fn new(capacity: usize, num_dsk: usize, num_pips: usize) -> Self {
        Self {
            capacity,
            num_dsk,
            num_pips,
        }
    }

    /// `sink_{idx}`, the name of a compositor sink pad.
    pub fn sink(idx: usize) -> String {
        format!("sink_{}", idx)
    }

    /// Whether border underlay pads exist (they are only built with PiPs).
    fn has_underlays(&self) -> bool {
        self.num_pips > 0
    }

    // --- Distribution compositor ---

    /// Dist pad of input `i`. Always `i`: transitions, the T-bar and the
    /// stinger address a dist input pad by its input index, so this is an
    /// invariant, not a detail of the layout.
    pub fn dist_input(&self, i: usize) -> usize {
        i
    }

    /// The input a dist pad index belongs to, if it is an input pad.
    pub fn dist_input_of(&self, idx: usize) -> Option<usize> {
        (idx < self.capacity).then_some(idx)
    }

    /// Dist pad of DSK layer `d`.
    pub fn dist_dsk(&self, d: usize) -> usize {
        self.capacity + d
    }

    /// The DSK layer a dist pad index belongs to, if it is a DSK pad.
    pub fn dist_dsk_of(&self, idx: usize) -> Option<usize> {
        idx.checked_sub(self.capacity).filter(|d| *d < self.num_dsk)
    }

    /// First dist border underlay pad (input `i` at base + i), if built.
    pub fn dist_underlay_base(&self) -> Option<usize> {
        self.has_underlays().then_some(self.capacity + self.num_dsk)
    }

    /// Dist pad of the stinger graphic; the matte (GPU only) is the next one.
    /// Stinger pads come after the inputs, the DSKs and the border underlays.
    pub fn dist_stinger(&self) -> usize {
        self.capacity
            + self.num_dsk
            + if self.has_underlays() {
                self.capacity
            } else {
                0
            }
    }

    /// Dist pad of the stinger matte (GPU only).
    pub fn dist_stinger_matte(&self) -> usize {
        self.dist_stinger() + 1
    }

    // --- Multiview compositor ---

    /// Multiview thumbnail pad of input `i`.
    pub fn mv_thumb(&self, i: usize) -> usize {
        i
    }

    /// Multiview PGM big display pad (fed from the program output).
    pub fn mv_pgm(&self) -> usize {
        self.capacity
    }

    /// First multiview PVW big candidate pad (input `i` at base + i).
    pub fn mv_pvw_base(&self) -> usize {
        self.capacity + 1
    }

    /// Multiview PVW big candidate pad of input `i`.
    pub fn mv_pvw(&self, i: usize) -> usize {
        self.mv_pvw_base() + i
    }

    /// First multiview pad of PiP tile `pip` (input `i` at base + i).
    pub fn mv_pip_base(&self, pip: usize) -> usize {
        2 * self.capacity + 1 + pip * self.capacity
    }

    /// Multiview pad of input `i` in PiP tile `pip`.
    pub fn mv_pip(&self, pip: usize, i: usize) -> usize {
        self.mv_pip_base(pip) + i
    }

    /// Multiview cairo overlay pad.
    pub fn mv_overlay(&self) -> usize {
        2 * self.capacity + 1 + self.num_pips * self.capacity
    }

    /// First multiview PVW border underlay pad (input `i` at base + i), if built.
    pub fn mv_pvw_underlay_base(&self) -> Option<usize> {
        self.has_underlays().then(|| self.mv_overlay() + 1)
    }

    /// First multiview border underlay pad of PiP tile `pip`, if built.
    pub fn mv_pip_underlay_base(&self, pip: usize) -> Option<usize> {
        self.mv_pvw_underlay_base()
            .map(|b| b + self.capacity * (1 + pip))
    }

    // --- Preview output compositor ---

    /// Preview output pad of input `i`.
    pub fn pvw_out_input(&self, i: usize) -> usize {
        i
    }

    /// First preview output border underlay pad (input `i` at base + i), if built.
    pub fn pvw_out_underlay_base(&self) -> Option<usize> {
        self.has_underlays().then_some(self.capacity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The index formulas as they were written out inline before `PadMap`
    /// (N = num_inputs, D = DSKs, P = PiPs). With capacity == num_inputs the
    /// map must name exactly the same pads, so existing flows keep their pad
    /// names.
    struct Inline {
        n: usize,
        d: usize,
        p: usize,
    }

    impl Inline {
        fn dist_dsk(&self, i: usize) -> usize {
            self.n + i
        }
        fn dist_underlay(&self, i: usize) -> Option<usize> {
            (self.p > 0).then(|| self.n + self.d + i)
        }
        fn dist_stinger(&self) -> usize {
            self.n + self.d + if self.p > 0 { self.n } else { 0 }
        }
        fn mv_pgm(&self) -> usize {
            self.n
        }
        fn mv_pvw(&self, i: usize) -> usize {
            self.n + 1 + i
        }
        fn mv_pip(&self, pip: usize, i: usize) -> usize {
            2 * self.n + 1 + pip * self.n + i
        }
        fn mv_overlay(&self) -> usize {
            2 * self.n + 1 + self.p * self.n
        }
        fn mv_pvw_underlay(&self, i: usize) -> Option<usize> {
            (self.p > 0).then(|| 2 * self.n + 2 + self.p * self.n + i)
        }
        fn mv_pip_underlay(&self, pip: usize, i: usize) -> Option<usize> {
            (self.p > 0).then(|| self.mv_overlay() + 1 + self.n * (1 + pip) + i)
        }
        fn pvw_out_underlay(&self, i: usize) -> Option<usize> {
            (self.p > 0).then(|| self.n + i)
        }
    }

    #[test]
    fn capacity_equal_to_num_inputs_gives_the_inline_pad_names() {
        for n in [2, 3, 4, 8, 17, 32] {
            for d in [0, 1, 9] {
                for p in [0, 1, 4] {
                    let old = Inline { n, d, p };
                    let map = PadMap::new(n, d, p);
                    for i in 0..n {
                        assert_eq!(map.dist_input(i), i);
                        assert_eq!(map.dist_input_of(i), Some(i));
                        assert_eq!(map.mv_thumb(i), i);
                        assert_eq!(map.mv_pvw(i), old.mv_pvw(i));
                        assert_eq!(map.pvw_out_input(i), i);
                        assert_eq!(
                            map.dist_underlay_base().map(|b| b + i),
                            old.dist_underlay(i)
                        );
                        assert_eq!(
                            map.mv_pvw_underlay_base().map(|b| b + i),
                            old.mv_pvw_underlay(i)
                        );
                        assert_eq!(
                            map.pvw_out_underlay_base().map(|b| b + i),
                            old.pvw_out_underlay(i)
                        );
                        for pip in 0..p {
                            assert_eq!(map.mv_pip(pip, i), old.mv_pip(pip, i));
                            assert_eq!(
                                map.mv_pip_underlay_base(pip).map(|b| b + i),
                                old.mv_pip_underlay(pip, i)
                            );
                        }
                    }
                    for k in 0..d {
                        assert_eq!(map.dist_dsk(k), old.dist_dsk(k));
                        assert_eq!(map.dist_dsk_of(old.dist_dsk(k)), Some(k));
                    }
                    assert_eq!(map.dist_input_of(n), None);
                    assert_eq!(map.dist_dsk_of(n + d), None);
                    assert_eq!(map.dist_stinger(), old.dist_stinger());
                    assert_eq!(map.dist_stinger_matte(), old.dist_stinger() + 1);
                    assert_eq!(map.mv_pgm(), old.mv_pgm());
                    assert_eq!(map.mv_overlay(), old.mv_overlay());
                    assert_eq!(map.mv_pvw_base(), old.mv_pvw(0));
                }
            }
        }
    }

    /// No two pads of one compositor share an index, whatever the capacity.
    #[test]
    fn indices_on_one_compositor_are_distinct() {
        for c in [2, 5, 16] {
            for d in [0, 2] {
                for p in [0, 3] {
                    let map = PadMap::new(c, d, p);
                    let mut dist: Vec<usize> = (0..c).map(|i| map.dist_input(i)).collect();
                    dist.extend((0..d).map(|k| map.dist_dsk(k)));
                    dist.extend(map.dist_underlay_base().into_iter().flat_map(|b| b..b + c));
                    dist.push(map.dist_stinger());
                    dist.push(map.dist_stinger_matte());
                    let mut mv: Vec<usize> = (0..c).map(|i| map.mv_thumb(i)).collect();
                    mv.push(map.mv_pgm());
                    mv.extend((0..c).map(|i| map.mv_pvw(i)));
                    mv.extend((0..p).flat_map(|pip| (0..c).map(move |i| map.mv_pip(pip, i))));
                    mv.push(map.mv_overlay());
                    mv.extend(
                        map.mv_pvw_underlay_base()
                            .into_iter()
                            .flat_map(|b| b..b + c),
                    );
                    mv.extend((0..p).flat_map(|pip| {
                        map.mv_pip_underlay_base(pip)
                            .into_iter()
                            .flat_map(|b| b..b + c)
                    }));
                    for list in [dist, mv] {
                        let mut sorted = list.clone();
                        sorted.sort_unstable();
                        sorted.dedup();
                        assert_eq!(sorted.len(), list.len(), "c={c} d={d} p={p}");
                        // Contiguous from 0: the GL mixer names pads in link order.
                        assert_eq!(sorted.last().copied(), Some(list.len() - 1));
                    }
                }
            }
        }
    }
}
