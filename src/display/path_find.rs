// Paths over the map's pixel locations along the tracks (A*), honoring
// transit modes and track directions (no U-turns, no switching track sets)
use esp_hal::rng::Rng;

use crate::{
    config::{CONFIG, LocPixNode},
    util::NonMax,
};

const LOC_COUNT: usize = CONFIG.cfg.loc_pix_nodes.len();
const ANY_DIR: u16 = u16::MAX;

#[derive(Copy, Clone, Default)]
struct OpenLocNode {
    idx: u16,
    f: i64,
    modes: u8,
    dir: u16,
}

#[derive(Copy, Clone, Default)]
struct ClosedLocNode {
    dir: u16,
}

#[derive(Copy, Clone)]
struct CameFromLocNode {
    loc_idx: NonMax<u16>,
    edge_idx: u8,
}

impl Default for CameFromLocNode {
    fn default() -> Self {
        CameFromLocNode {
            loc_idx: NonMax::NONE,
            edge_idx: 0,
        }
    }
}

#[derive(Copy, Clone, Default)]
pub struct LocPixEdgeDirected {
    pub from_loc: u16,
    pub from_pix: u16,
}

#[derive(defmt::Format)]
pub enum PathFindError {
    OutOfMemory,
    NoPathFound,
}

impl core::fmt::Display for PathFindError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PathFindError::OutOfMemory => write!(f, "OutOfMemory"),
            PathFindError::NoPathFound => write!(f, "NoPathFound"),
        }
    }
}

// Arrival direction and transit modes at the end of a path
#[derive(Copy, Clone)]
struct PathEndState {
    dir: u16,
    modes: u8,
}

struct SearchBuffers<const MAX_OPEN: usize> {
    open_heap: [OpenLocNode; MAX_OPEN],
    g_score: [i64; LOC_COUNT],
    came_from: [CameFromLocNode; LOC_COUNT],
    closed_set: [ClosedLocNode; LOC_COUNT],
}

/// Finds paths with fixed-size buffers: at most MAX_OPEN open nodes, and
/// paths of at most MAX_PATH locations.
pub struct PathFinder<const MAX_OPEN: usize, const MAX_PATH: usize> {
    search: SearchBuffers<MAX_OPEN>,
    path_buf: [LocPixEdgeDirected; MAX_PATH],
}

impl<const MAX_OPEN: usize, const MAX_PATH: usize> PathFinder<MAX_OPEN, MAX_PATH> {
    pub fn new() -> Self {
        PathFinder {
            search: SearchBuffers {
                open_heap: [Default::default(); MAX_OPEN],
                g_score: [0; LOC_COUNT],
                came_from: [Default::default(); LOC_COUNT],
                closed_set: [Default::default(); LOC_COUNT],
            },
            path_buf: [Default::default(); MAX_PATH],
        }
    }

    /// The shortest path from start to goal, through via if given. Each entry
    /// is a location with the pixel the path leaves it at (the goal: arrives).
    pub fn find(
        &mut self,
        start_loc_idx: u16,
        via_loc_idx: Option<u16>,
        goal_loc_idx: u16,
    ) -> Result<&[LocPixEdgeDirected], PathFindError> {
        // Via location is only meaningful if it is distinct from start and goal
        let Some(via_loc_idx) =
            via_loc_idx.filter(|&via| via != start_loc_idx && via != goal_loc_idx)
        else {
            let (len, _) = find_segment(
                &mut self.search,
                start_loc_idx,
                goal_loc_idx,
                None,
                &mut self.path_buf,
            )?;
            return Ok(&self.path_buf[..len]);
        };

        let (len_first, via_state) = find_segment(
            &mut self.search,
            start_loc_idx,
            via_loc_idx,
            None,
            &mut self.path_buf,
        )?;

        // Continuing with the arrival direction and transit modes at via (no
        // U-turn or change of modes there). Overwrites the first segment's last
        // entry (via, with its incoming pixel) with via and its outgoing pixel
        let (len_second, _) = find_segment(
            &mut self.search,
            via_loc_idx,
            goal_loc_idx,
            Some(via_state),
            &mut self.path_buf[len_first - 1..],
        )?;

        Ok(&self.path_buf[..len_first - 1 + len_second])
    }
}

fn find_segment<const MAX_OPEN: usize>(
    search: &mut SearchBuffers<MAX_OPEN>,
    start_loc_idx: u16,
    goal_loc_idx: u16,
    start_state: Option<PathEndState>,
    path_buf: &mut [LocPixEdgeDirected],
) -> Result<(usize, PathEndState), PathFindError> {
    let loc_nodes = CONFIG.cfg.loc_pix_nodes;
    let SearchBuffers {
        open_heap,
        g_score,
        came_from,
        closed_set,
    } = search;
    g_score.fill(i64::MAX);
    came_from.fill(CameFromLocNode::default());
    closed_set.fill(ClosedLocNode::default());

    let start_loc = &loc_nodes[start_loc_idx as usize];
    let goal_loc = &loc_nodes[goal_loc_idx as usize];

    let mut heap_len = 0;
    g_score[start_loc_idx as usize] = 0;
    heap_push(
        open_heap,
        &mut heap_len,
        OpenLocNode {
            idx: start_loc_idx,
            f: 0,
            modes: start_loc.modes & goal_loc.modes & start_state.map_or(u8::MAX, |s| s.modes),
            dir: start_state.map_or(ANY_DIR, |s| s.dir),
        },
    )?;

    while heap_len > 0 {
        let current = heap_pop(open_heap, &mut heap_len);
        let cur_idx = current.idx as usize;

        if current.idx == goal_loc_idx {
            let len = reconstruct_path(came_from, goal_loc_idx, path_buf)?;
            return Ok((
                len,
                PathEndState {
                    dir: current.dir,
                    modes: current.modes,
                },
            ));
        }

        if closed_set[cur_idx].dir & current.dir != 0 {
            continue;
        }
        closed_set[cur_idx].dir |= current.dir;

        let cur_loc = &loc_nodes[cur_idx];
        for (edge_idx, edge) in cur_loc.edges.iter().enumerate() {
            let succ = edge.to_loc as usize;
            let succ_loc = &loc_nodes[succ];

            for succ_rev_dir in reverse_dirs(succ_loc, current.idx) {
                if closed_set[succ].dir & succ_rev_dir != 0 {
                    continue;
                }
                // E.g. subway can't route to a light-rail-only location
                let mode_overlap = current.modes & succ_loc.modes;
                if mode_overlap == 0 {
                    continue;
                }
                if current.dir != ANY_DIR && !is_track_compatible(current.dir, edge.dir) {
                    continue;
                }

                // Manhattan distances: step cost, and heuristic to the goal
                let step_cost = (cur_loc.lat_e7 as i64 - succ_loc.lat_e7 as i64).abs()
                    + (cur_loc.lng_e7 as i64 - succ_loc.lng_e7 as i64).abs();
                let tentative_g = g_score[cur_idx] + step_cost;
                came_from[succ] = CameFromLocNode {
                    loc_idx: NonMax::new(cur_idx as u16).unwrap(),
                    edge_idx: edge_idx as u8,
                };
                g_score[succ] = tentative_g;
                let h = (succ_loc.lat_e7 as i64 - goal_loc.lat_e7 as i64).abs()
                    + (succ_loc.lng_e7 as i64 - goal_loc.lng_e7 as i64).abs();

                heap_push(
                    open_heap,
                    &mut heap_len,
                    OpenLocNode {
                        idx: edge.to_loc,
                        f: tentative_g + h,
                        modes: mode_overlap,
                        dir: succ_rev_dir,
                    },
                )?;
            }
        }
    }

    Err(PathFindError::NoPathFound)
}

// Follows came_from back from the goal, into path_buf in path order
fn reconstruct_path(
    came_from: &[CameFromLocNode],
    goal_loc_idx: u16,
    path_buf: &mut [LocPixEdgeDirected],
) -> Result<usize, PathFindError> {
    let loc_nodes = CONFIG.cfg.loc_pix_nodes;
    let mut len = 0;
    let mut cur = CameFromLocNode {
        loc_idx: NonMax::new(goal_loc_idx).unwrap(),
        edge_idx: 0,
    };

    while let Some(cur_idx) = cur.loc_idx.as_option() {
        if len >= path_buf.len() {
            return Err(PathFindError::OutOfMemory);
        }
        let from_pix = if len == 0 {
            // The goal: the pixel the previous edge arrives at
            let prev = came_from[cur_idx as usize];
            let Some(prev_loc_idx) = prev.loc_idx.as_option() else {
                return Err(PathFindError::NoPathFound);
            };
            loc_nodes[prev_loc_idx as usize].edges[prev.edge_idx as usize].to_pix
        } else {
            loc_nodes[cur_idx as usize].edges[cur.edge_idx as usize].from_pix
        };
        path_buf[len] = LocPixEdgeDirected {
            from_loc: cur_idx,
            from_pix,
        };
        cur = came_from[cur_idx as usize];
        len += 1;
    }

    path_buf[..len].reverse();
    Ok(len)
}

// The directions of the location's edges back to from_loc_idx
fn reverse_dirs(loc: &'static LocPixNode, from_loc_idx: u16) -> impl Iterator<Item = u16> {
    loc.edges
        .iter()
        .filter(move |edge| edge.to_loc == from_loc_idx)
        .map(|edge| edge.dir)
}

// No U-turn (outgoing direction differs from incoming), and no switch to
// another track set
fn is_track_compatible(incoming_dir: u16, outgoing_dir: u16) -> bool {
    incoming_dir & outgoing_dir == 0
        && incoming_dir.trailing_zeros() / 4 == outgoing_dir.trailing_zeros() / 4
}

struct LocPixEdgeStep {
    to_loc: u16,
    from_pix: u16,
    to_pix: u16,
    dir: u16,
}

/// Moves one random step along the tracks from the location, updating the
/// location, direction and allowed modes. The pixels stepped from and to.
pub fn do_random_step_from_pixel_location(
    cur_loc_idx: &mut u16,
    cur_dir_opt: &mut NonMax<u16>,
    modes: &mut u8,
    rng: &mut Rng,
) -> Option<(u16, u16)> {
    let loc_nodes = CONFIG.cfg.loc_pix_nodes;
    let cur_loc = &loc_nodes[*cur_loc_idx as usize];

    let valid_edges: heapless::Vec<LocPixEdgeStep, 10> = cur_loc
        .edges
        .iter()
        .filter_map(|edge| {
            let succ_loc = &loc_nodes[edge.to_loc as usize];
            if *modes & succ_loc.modes == 0 {
                return None;
            }
            let is_compatible = cur_dir_opt
                .as_option()
                .is_none_or(|cur_dir| is_track_compatible(cur_dir, edge.dir));
            if !is_compatible {
                return None;
            }
            reverse_dirs(succ_loc, *cur_loc_idx)
                .next()
                .map(|succ_rev_dir| LocPixEdgeStep {
                    to_loc: edge.to_loc,
                    from_pix: edge.from_pix,
                    to_pix: edge.to_pix,
                    dir: succ_rev_dir,
                })
        })
        .collect();

    if valid_edges.is_empty() {
        return None;
    }
    let chosen_edge = &valid_edges[rng.random() as usize % valid_edges.len()];
    *cur_dir_opt = NonMax::new(chosen_edge.dir).unwrap();
    *cur_loc_idx = chosen_edge.to_loc;
    *modes &= loc_nodes[chosen_edge.to_loc as usize].modes & cur_loc.modes;
    Some((chosen_edge.from_pix, chosen_edge.to_pix))
}

// Binary min-heap by f, in heap[..heap_len]
fn heap_push(
    heap: &mut [OpenLocNode],
    heap_len: &mut usize,
    node: OpenLocNode,
) -> Result<(), PathFindError> {
    if *heap_len >= heap.len() {
        return Err(PathFindError::OutOfMemory);
    }

    let mut i = *heap_len;
    heap[i] = node;
    *heap_len += 1;

    while i > 0 {
        let p = (i - 1) >> 1;
        if heap[p].f <= heap[i].f {
            break;
        }
        heap.swap(p, i);
        i = p;
    }
    Ok(())
}

fn heap_pop(heap: &mut [OpenLocNode], heap_len: &mut usize) -> OpenLocNode {
    let root = heap[0];
    *heap_len -= 1;
    heap[0] = heap[*heap_len];

    let mut i = 0;
    loop {
        let l = i * 2 + 1;
        let r = l + 1;
        if l >= *heap_len {
            break;
        }
        let c = if r < *heap_len && heap[r].f < heap[l].f {
            r
        } else {
            l
        };
        if heap[i].f <= heap[c].f {
            break;
        }
        heap.swap(i, c);
        i = c;
    }
    root
}
