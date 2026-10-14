// Mapping of the transit data's stations to the map's pixel locations
use crate::{config::CONFIG, trace, util::NonMax};

const MAX_STOP_TO_LOC_DIST_DEG_SQ_E7: i64 = 4_000_000_000; // < 1km

/// The pixel location of the station. None if it isn't on the map.
pub fn station_location(stop_idx: u16, latitude_e7: i32, longitude_e7: i32) -> Option<NonMax<u16>> {
    let Some(loc_id) = lookup_nearest_location(latitude_e7, longitude_e7) else {
        trace::err!(
            "Stop ID {} at ({}, {}) has no nearby pixel location",
            stop_idx,
            latitude_e7,
            longitude_e7
        );
        return None;
    };

    let loc = &CONFIG.cfg.loc_pix_nodes[loc_id as usize];
    let dlat = latitude_e7 as i64 - loc.lat_e7 as i64;
    let dlng = longitude_e7 as i64 - loc.lng_e7 as i64;
    if dlat * dlat + dlng * dlng > MAX_STOP_TO_LOC_DIST_DEG_SQ_E7 {
        trace::err!(
            "Stop ID {} at ({}, {}) too far from nearest Loc ID {} at ({}, {})",
            stop_idx,
            latitude_e7,
            longitude_e7,
            loc_id,
            loc.lat_e7,
            loc.lng_e7
        );
        return None;
    }

    let location = NonMax::new(loc_id);
    if location.is_none() {
        trace::err!(
            "Loc ID {} for stop ID {} at ({}, {}) is invalid",
            loc_id,
            stop_idx,
            latitude_e7,
            longitude_e7
        );
    }
    location
}

/// The pixel location nearest to the coordinates, by an iterative search of
/// the product's K-D tree (split by latitude at even depths, longitude at odd).
fn lookup_nearest_location(latitude_e7: i32, longitude_e7: i32) -> Option<u16> {
    let kd_tree = CONFIG.cfg.loc_geo_kd_tree;
    let loc_nodes = CONFIG.cfg.loc_pix_nodes;

    let mut best_dist_sq: i64 = i64::MAX;
    let mut best_loc_idx: Option<u16> = None;

    // (node index, depth)
    let mut stack: heapless::Vec<(usize, usize), 32> = heapless::Vec::new();
    stack.push((0, 0)).unwrap();

    while let Some((node_idx, depth)) = stack.pop() {
        let kd_node = &kd_tree[node_idx];
        let loc_node = &loc_nodes[kd_node.loc as usize];

        let dlat = latitude_e7 as i64 - loc_node.lat_e7 as i64;
        let dlng = longitude_e7 as i64 - loc_node.lng_e7 as i64;
        let dist_sq = dlat * dlat + dlng * dlng;
        if dist_sq < best_dist_sq {
            best_dist_sq = dist_sq;
            best_loc_idx = Some(kd_node.loc);
        }

        let (query_coord, node_coord) = if depth % 2 == 0 {
            (latitude_e7, loc_node.lat_e7)
        } else {
            (longitude_e7, loc_node.lng_e7)
        };
        let diff = (query_coord - node_coord) as i64;
        let (near, far) = if query_coord < node_coord {
            (kd_node.left, kd_node.right)
        } else {
            (kd_node.right, kd_node.left)
        };

        if let Some(idx) = near {
            stack.push((idx as usize, depth + 1)).unwrap();
        }
        // The far side only if it can hold a closer location
        if diff * diff < best_dist_sq
            && let Some(idx) = far
        {
            stack.push((idx as usize, depth + 1)).unwrap();
        }
    }

    best_loc_idx
}
