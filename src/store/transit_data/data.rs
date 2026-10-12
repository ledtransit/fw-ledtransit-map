// The transit data as kept in memory. The vehicles' segments and the segments'
// via stops are each kept in one array, referenced by span, instead of an
// allocation per vehicle and per segment (as decoded): each array is allocated
// to its size, without the allocations' overhead and spare capacity
use alloc::{string::String, vec::Vec};

use crate::net::ws_client::client_proto::{self, Disruption, Line, Stop, ViaStop};

#[derive(Default)]
pub struct TransitData {
    pub sourced_at_unix_timestamp: u32,
    pub simulated_until_unix_timestamp: u32,
    pub feed_source: String,
    pub lines: Vec<Line>,
    pub stops: Vec<Stop>,
    pub vehicle_movements: Vec<VehicleMovement>,
    pub disruptions: Vec<Disruption>,
    segments: Vec<MovementSegment>,
    via_stops: Vec<ViaStop>,
}

/// A vehicle's movement, its segments in [`TransitData::segments`].
pub struct VehicleMovement {
    pub line_id_x_trip_id: u32, // u16 + u16
    segments: Span,
}

/// A segment of a movement (fields as in the protocol), its via stops in
/// [`TransitData::via_stops`].
pub struct MovementSegment {
    pub from_stop_id_x_to_stop_id: u32,            // u16 + u16
    pub move_seconds_x_wait_seconds: u32,          // u16 + u16
    pub delayed_seconds_x_average_speed_kmph: i32, // i16 + u16
    pub canceled_x_start_offset_seconds_x_via_total_move_seconds: u32, // bool + u15 + u16
    via_stops: Span,
}

// A range of one of the arrays
#[derive(Clone, Copy)]
struct Span {
    start: u16,
    len: u16,
}

/// More segments or via stops than a span can reference.
#[derive(Debug, defmt::Format)]
pub struct TooLargeError;

impl Span {
    // The elements to come, from the array's current length on
    fn following<T>(items: &[T], len: usize) -> Result<Self, TooLargeError> {
        let start = u16::try_from(items.len()).map_err(|_| TooLargeError)?;
        let len = u16::try_from(len).map_err(|_| TooLargeError)?;
        start.checked_add(len).ok_or(TooLargeError)?;
        Ok(Self { start, len })
    }

    fn of<'a, T>(&self, items: &'a [T]) -> &'a [T] {
        let start = usize::from(self.start);
        &items[start..start + usize::from(self.len)]
    }
}

impl TransitData {
    pub fn segments(&self, vehicle: &VehicleMovement) -> &[MovementSegment] {
        vehicle.segments.of(&self.segments)
    }

    pub fn via_stops(&self, segment: &MovementSegment) -> &[ViaStop] {
        segment.via_stops.of(&self.via_stops)
    }

    /// Adds the vehicle (as decoded), with its segments and their via stops.
    pub fn push_vehicle(
        &mut self,
        vehicle: client_proto::VehicleMovement,
    ) -> Result<(), TooLargeError> {
        let segments = Span::following(&self.segments, vehicle.segments.len())?;
        for segment in vehicle.segments {
            let via_stops = Span::following(&self.via_stops, segment.via_stops.len())?;
            self.via_stops.extend(segment.via_stops);
            self.segments.push(MovementSegment {
                from_stop_id_x_to_stop_id: segment.from_stop_id_x_to_stop_id,
                move_seconds_x_wait_seconds: segment.move_seconds_x_wait_seconds,
                delayed_seconds_x_average_speed_kmph: segment.delayed_seconds_x_average_speed_kmph,
                canceled_x_start_offset_seconds_x_via_total_move_seconds: segment
                    .canceled_x_start_offset_seconds_x_via_total_move_seconds,
                via_stops,
            });
        }
        self.vehicle_movements.push(VehicleMovement {
            line_id_x_trip_id: vehicle.line_id_x_trip_id,
            segments,
        });
        Ok(())
    }

    /// Frees the arrays' spare capacity, once everything is added.
    pub fn shrink_to_fit(&mut self) {
        self.lines.shrink_to_fit();
        self.stops.shrink_to_fit();
        self.vehicle_movements.shrink_to_fit();
        self.disruptions.shrink_to_fit();
        self.segments.shrink_to_fit();
        self.via_stops.shrink_to_fit();
    }

    /// The bytes of the heap it takes (without the allocator's overhead).
    pub fn heap_size(&self) -> usize {
        fn size_of_vec<T>(items: &Vec<T>) -> usize {
            items.capacity() * size_of::<T>()
        }
        self.feed_source.capacity()
            + size_of_vec(&self.lines)
            + self
                .lines
                .iter()
                .map(|line| line.name.capacity())
                .sum::<usize>()
            + size_of_vec(&self.stops)
            + size_of_vec(&self.vehicle_movements)
            + size_of_vec(&self.disruptions)
            + size_of_vec(&self.segments)
            + size_of_vec(&self.via_stops)
    }
}

/// From all of it decoded at once (a message that fit the receive buffer).
impl TryFrom<client_proto::TransitData> for TransitData {
    type Error = TooLargeError;

    fn try_from(data: client_proto::TransitData) -> Result<Self, Self::Error> {
        let mut transit_data = TransitData {
            sourced_at_unix_timestamp: data.sourced_at_unix_timestamp,
            simulated_until_unix_timestamp: data.simulated_until_unix_timestamp,
            feed_source: data.feed_source,
            lines: data.lines,
            stops: data.stops,
            disruptions: data.disruptions,
            ..Default::default()
        };
        for vehicle in data.vehicle_movements {
            transit_data.push_vehicle(vehicle)?;
        }
        transit_data.shrink_to_fit();
        Ok(transit_data)
    }
}
