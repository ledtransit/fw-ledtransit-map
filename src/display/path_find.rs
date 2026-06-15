use esp_hal::rng::Rng;

use crate::{config::CONFIG, util::NonMax};

#[derive(Copy, Clone, Default)]
pub struct OpenLocNode {
    idx: u16,
    f: i64,
    modes: u8,
    dir: u16,
}

#[derive(Copy, Clone, Default)]
pub struct ClosedLocNode {
    dir: u16,
}

#[derive(Copy, Clone)]
pub struct CameFromLocNode {
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

pub fn find_shortest_path_between_pixel_locations<'a>(
    start_loc_idx: u16,
    goal_loc_idx: u16,
    open_heap: &mut [OpenLocNode],
    g_score: &mut [i64],
    came_from: &mut [CameFromLocNode],
    closed_set: &mut [ClosedLocNode],
    path_buf: &'a mut [LocPixEdgeDirected],
) -> Result<&'a [LocPixEdgeDirected], PathFindError> {
    let loc_nodes = &CONFIG.cfg.loc_pix_nodes;
    let num_locs = loc_nodes.len();

    // Init
    for i in 0..num_locs {
        g_score[i] = i64::MAX;
        came_from[i] = CameFromLocNode::default();
        closed_set[i] = ClosedLocNode::default();
    }

    let start_loc = &loc_nodes[start_loc_idx as usize];
    let start_modes = start_loc.modes;

    let goal_loc = &loc_nodes[goal_loc_idx as usize];
    let goal_modes = goal_loc.modes;
    let (goal_lat, goal_lng) = (goal_loc.lat_e7, goal_loc.lng_e7);

    let mut heap_len = 0;

    g_score[start_loc_idx as usize] = 0;
    if !heap_push(
        open_heap,
        &mut heap_len,
        OpenLocNode {
            idx: start_loc_idx,
            f: 0,
            modes: start_modes & goal_modes,
            dir: u16::MAX,
        },
    ) {
        return Err(PathFindError::OutOfMemory);
    }

    // A* loop
    while heap_len > 0 {
        let current = heap_pop(open_heap, &mut heap_len);
        let cur_idx = current.idx as usize;

        // Goal reached -> reconstruct
        if current.idx == goal_loc_idx {
            let mut len = 0;
            let mut cur = CameFromLocNode {
                loc_idx: NonMax::new(goal_loc_idx).unwrap(),
                edge_idx: 0,
            };

            while let Some(cur_idx) = cur.loc_idx.as_option() {
                if len >= path_buf.len() {
                    return Err(PathFindError::OutOfMemory);
                }

                path_buf[len] = LocPixEdgeDirected {
                    from_loc: cur_idx,
                    from_pix: if len == 0 {
                        // Goal node -> use previous edge's to_pix instead
                        let prev = came_from[cur_idx as usize];
                        if prev.loc_idx.as_option().is_none() {
                            return Err(PathFindError::NoPathFound);
                        }
                        loc_nodes[prev.loc_idx.as_option().unwrap() as usize].edges
                            [prev.edge_idx as usize]
                            .to_pix
                    } else {
                        loc_nodes[cur_idx as usize].edges[cur.edge_idx as usize].from_pix
                    },
                };

                cur = came_from[cur_idx as usize];
                len += 1;
            }

            path_buf[..len].reverse();
            return Ok(&path_buf[..len]);
        }

        // Check already closed this direction
        if closed_set[cur_idx].dir & current.dir != 0 {
            continue;
        }
        closed_set[cur_idx].dir |= current.dir;

        let cur_loc = &loc_nodes[cur_idx];
        let (cur_lat, cur_lng) = (cur_loc.lat_e7, cur_loc.lng_e7);

        // Iterate neighbors
        for (edge_idx, edge) in cur_loc.edges.iter().enumerate() {
            let succ = edge.to_loc as usize;

            let succ_loc = &loc_nodes[succ];
            let succ_modes = succ_loc.modes;
            let (succ_lat, succ_lng) = (succ_loc.lat_e7, succ_loc.lng_e7);

            // Find reverse connection direction in bidirectional graph
            let succ_rev_dirs = succ_loc
                .edges
                .iter()
                .filter(|e| e.to_loc == current.idx)
                .map(|e| e.dir);

            for succ_rev_dir in succ_rev_dirs {
                // Check already closed this direction
                if closed_set[succ].dir & succ_rev_dir != 0 {
                    continue;
                }

                // Check transit mode compatibility (e.g. subway cannot route to light-rail-only location)
                let mode_overlap = current.modes & succ_modes;
                if mode_overlap == 0 {
                    continue;
                }

                // Check track direction compatibility (e.g. train cannot physically take this turn)
                if current.dir != u16::MAX {
                    if current.dir & edge.dir != 0 {
                        continue; // Outgoing direction must be different from incoming direction (cannot U-turn)
                    }
                    let from_track_set = current.dir.trailing_zeros() / 4;
                    let to_track_set = edge.dir.trailing_zeros() / 4;
                    if from_track_set != to_track_set {
                        continue; // Outgoing track must be part of same track set as incoming track (cannot switch tracks if not allowed)
                    }
                }

                // Define step cost as manhattan distance
                let delta_lat = cur_lat as i64 - succ_lat as i64;
                let delta_lng = cur_lng as i64 - succ_lng as i64;
                let step_cost = delta_lat.abs() + delta_lng.abs();
                let tentative_g = g_score[cur_idx] + step_cost;

                // Record best path to successor
                came_from[succ] = CameFromLocNode {
                    loc_idx: NonMax::new(cur_idx as u16).unwrap(),
                    edge_idx: edge_idx as u8,
                };
                g_score[succ] = tentative_g;

                // Compute heuristic (Manhattan distance to goal)
                let heu_lat = succ_lat as i64 - goal_lat as i64;
                let heu_lng = succ_lng as i64 - goal_lng as i64;
                let h = heu_lat.abs() + heu_lng.abs();

                if !heap_push(
                    open_heap,
                    &mut heap_len,
                    OpenLocNode {
                        idx: edge.to_loc,
                        f: tentative_g + h,
                        modes: mode_overlap,
                        dir: succ_rev_dir,
                    },
                ) {
                    return Err(PathFindError::OutOfMemory);
                }
            }
        }
    }

    Err(PathFindError::NoPathFound)
}

struct LocPixEdgeStep {
    to_loc: u16,
    from_pix: u16,
    to_pix: u16,
    dir: u16,
}

pub fn do_random_step_from_pixel_location(
    cur_loc_idx: &mut u16,
    cur_dir_opt: &mut NonMax<u16>,
    modes: &mut u8,
    rng: &mut Rng,
) -> Option<(u16, u16)> {
    let loc_nodes = &CONFIG.cfg.loc_pix_nodes;
    let cur_loc = &loc_nodes[*cur_loc_idx as usize];

    let valid_edges: heapless::Vec<LocPixEdgeStep, 10> = cur_loc
        .edges
        .iter()
        .filter_map(|edge| {
            let succ_loc = &loc_nodes[edge.to_loc as usize];
            let mode_overlap = *modes & succ_loc.modes;

            // Check transit mode compatibility
            if mode_overlap == 0 {
                return None;
            }

            let succ_rev_dirs = succ_loc
                .edges
                .iter()
                .filter(|e| e.to_loc == *cur_loc_idx)
                .map(|e| e.dir);
            for succ_rev_dir in succ_rev_dirs {
                // Check track direction compatibility
                if let Some(cur_dir) = cur_dir_opt.as_option() {
                    if cur_dir & edge.dir != 0 {
                        continue; // Outgoing direction must be different from incoming direction (cannot U-turn)
                    }
                    let from_track_set = cur_dir.trailing_zeros() / 4;
                    let to_track_set = edge.dir.trailing_zeros() / 4;
                    if from_track_set != to_track_set {
                        continue; // Outgoing track must be part of same track set as incoming track (cannot switch tracks if not allowed)
                    }
                }
                return Some(LocPixEdgeStep {
                    to_loc: edge.to_loc,
                    from_pix: edge.from_pix,
                    to_pix: edge.to_pix,
                    dir: succ_rev_dir,
                });
            }
            None
        })
        .collect();

    if valid_edges.is_empty() {
        None
    } else {
        let choice = rng.random() as usize % valid_edges.len();
        let chosen_edge = &valid_edges[choice];
        *cur_dir_opt = NonMax::new(chosen_edge.dir).unwrap();
        *cur_loc_idx = chosen_edge.to_loc;
        *modes &= loc_nodes[chosen_edge.to_loc as usize].modes & cur_loc.modes;
        Some((chosen_edge.from_pix, chosen_edge.to_pix))
    }
}

#[inline]
fn heap_push(heap: &mut [OpenLocNode], heap_len: &mut usize, node: OpenLocNode) -> bool {
    if *heap_len >= heap.len() {
        return false; // out of memory
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
    true
}

#[inline]
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
