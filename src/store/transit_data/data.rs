// The transit data as kept in memory: each value as narrow as its range (IDs
// are 16-bit indices into the lines and stops). The vehicles' segments and the
// segments' via stops are each kept in one array, referenced by span, instead
// of an allocation per vehicle and per segment (as decoded): each array is
// allocated to its size, without the allocations' overhead and spare capacity
use alloc::{string::String, vec::Vec};

use super::stops;
use crate::{
    net::ws_client::client_proto::{self, DisruptionType, Line},
    util::NonMax,
};

#[derive(Default)]
pub struct TransitData {
    pub sourced_at_unix_timestamp: u32,
    pub simulated_until_unix_timestamp: u32,
    pub feed_source: String,
    pub lines: Vec<Line>,
    pub stops: Vec<Stop>,
    /// The pixel location of each stop, if it's a station on the map.
    pub stop_locations: Vec<NonMax<u16>>,
    pub vehicle_movements: Vec<VehicleMovement>,
    pub disruptions: Vec<Disruption>,
    segments: Vec<MovementSegment>,
    via_stops: Vec<ViaStop>,
}

#[derive(Clone, Copy)]
pub struct Stop {
    pub latitude_e7: i32,
    pub longitude_e7: i32,
}

/// A vehicle's movement, its segments in [`TransitData::segments`].
pub struct VehicleMovement {
    pub line_id: u16,
    pub trip_id: u16,              // Stable unique ID
    pub start_offset_seconds: i16, // When the first segment starts, relative to sourced_at_unix_timestamp
    segments: Span,
}

/// A segment of a movement, its via stops in [`TransitData::via_stops`].
pub struct MovementSegment {
    pub from_stop_id: u16,
    pub to_stop_id: u16,
    pub move_seconds: u16, // Moving from from_stop_id to to_stop_id (all legs)
    pub wait_seconds: u16, // Then waiting at to_stop_id
    pub delayed_seconds: NonMax<i16>, // None: no real-time data
    pub average_speed_kmph: u16,
    pub cancelled: bool,
    via_stops: Span,
}

#[derive(Clone, Copy)]
pub struct ViaStop {
    pub stop_id: u16,
    pub move_seconds: u16, // Of the leg to this stop
}

/// A disruption (its stop count and whether it affects the entire line aren't
/// kept: not shown).
pub struct Disruption {
    pub line_id: u16,
    pub disruption_id: u16, // Stable unique ID
    pub from_stop_id: u16,
    pub to_stop_id: Option<u16>, // Else only from_stop_id is affected
    pub direction_hint_to_stop_id: Option<u16>,
    pub direction_hint_from_stop_id: Option<u16>,
    pub via_stop_id: Option<u16>,
    pub bidirectional: bool,
    pub affects_all_lines: bool,
    pub kind: DisruptionType,
}

/// A value out of its range (e.g. an ID over 16 bits), or more elements than
/// can be referenced.
#[derive(Debug, defmt::Format)]
pub struct InvalidDataError;

// A range of one of the arrays
#[derive(Clone, Copy)]
struct Span {
    start: u16,
    len: u16,
}

impl Span {
    // The elements to come, from the array's current length on
    fn following<T>(items: &[T], len: usize) -> Result<Self, InvalidDataError> {
        let start: u16 = narrow(items.len())?;
        let len: u16 = narrow(len)?;
        start.checked_add(len).ok_or(InvalidDataError)?;
        Ok(Self { start, len })
    }

    fn of<'a, T>(&self, items: &'a [T]) -> &'a [T] {
        let start = usize::from(self.start);
        &items[start..start + usize::from(self.len)]
    }
}

fn narrow<T, U: TryFrom<T>>(value: T) -> Result<U, InvalidDataError> {
    U::try_from(value).map_err(|_| InvalidDataError)
}

fn narrow_option<T, U: TryFrom<T>>(value: Option<T>) -> Result<Option<U>, InvalidDataError> {
    value.map(narrow).transpose()
}

impl TransitData {
    pub fn segments(&self, vehicle: &VehicleMovement) -> &[MovementSegment] {
        vehicle.segments.of(&self.segments)
    }

    pub fn via_stops(&self, segment: &MovementSegment) -> &[ViaStop] {
        segment.via_stops.of(&self.via_stops)
    }

    /// Adds the stop (as decoded), and its pixel location if it's a station.
    pub fn push_stop(&mut self, stop: client_proto::Stop) -> Result<(), InvalidDataError> {
        let stop_id: u16 = narrow(self.stops.len())?;
        let location = if stop.is_station {
            stops::station_location(stop_id, stop.latitude_e7, stop.longitude_e7)
                .unwrap_or(NonMax::NONE)
        } else {
            NonMax::NONE
        };
        self.stops.push(Stop {
            latitude_e7: stop.latitude_e7,
            longitude_e7: stop.longitude_e7,
        });
        self.stop_locations.push(location);
        Ok(())
    }

    /// Adds the vehicle (as decoded), with its segments and their via stops.
    pub fn push_vehicle(
        &mut self,
        vehicle: client_proto::VehicleMovement,
    ) -> Result<(), InvalidDataError> {
        let segments = Span::following(&self.segments, vehicle.segments.len())?;
        for segment in vehicle.segments {
            let via_stops = Span::following(&self.via_stops, segment.via_stops.len())?;
            for via_stop in segment.via_stops {
                self.via_stops.push(ViaStop {
                    stop_id: narrow(via_stop.stop_id)?,
                    move_seconds: narrow(via_stop.move_seconds)?,
                });
            }
            let delayed_seconds = match segment.delayed_seconds {
                Some(delayed_seconds) => {
                    NonMax::new(narrow(delayed_seconds)?).ok_or(InvalidDataError)?
                }
                None => NonMax::NONE,
            };
            self.segments.push(MovementSegment {
                from_stop_id: narrow(segment.from_stop_id)?,
                to_stop_id: narrow(segment.to_stop_id)?,
                move_seconds: narrow(segment.move_seconds)?,
                wait_seconds: narrow(segment.wait_seconds)?,
                delayed_seconds,
                average_speed_kmph: narrow(segment.average_speed_kmph)?,
                cancelled: segment.cancelled,
                via_stops,
            });
        }
        self.vehicle_movements.push(VehicleMovement {
            line_id: narrow(vehicle.line_id)?,
            trip_id: narrow(vehicle.trip_id)?,
            start_offset_seconds: narrow(vehicle.start_offset_seconds)?,
            segments,
        });
        Ok(())
    }

    /// Adds the disruption (as decoded).
    pub fn push_disruption(
        &mut self,
        disruption: client_proto::Disruption,
    ) -> Result<(), InvalidDataError> {
        self.disruptions.push(Disruption {
            line_id: narrow(disruption.line_id)?,
            disruption_id: narrow(disruption.disruption_id)?,
            from_stop_id: narrow(disruption.from_stop_id)?,
            to_stop_id: narrow_option(disruption.to_stop_id)?,
            direction_hint_to_stop_id: narrow_option(disruption.direction_hint_to_stop_id)?,
            direction_hint_from_stop_id: narrow_option(disruption.direction_hint_from_stop_id)?,
            via_stop_id: narrow_option(disruption.via_stop_id)?,
            bidirectional: disruption.bidirectional,
            affects_all_lines: disruption.affects_all_lines,
            kind: DisruptionType::try_from(disruption.r#type).map_err(|_| InvalidDataError)?,
        });
        Ok(())
    }

    /// Frees the arrays' spare capacity, once everything is added.
    pub fn shrink_to_fit(&mut self) {
        self.lines.shrink_to_fit();
        self.stops.shrink_to_fit();
        self.stop_locations.shrink_to_fit();
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
            + size_of_vec(&self.stop_locations)
            + size_of_vec(&self.vehicle_movements)
            + size_of_vec(&self.disruptions)
            + size_of_vec(&self.segments)
            + size_of_vec(&self.via_stops)
    }
}

/// From all of it decoded at once (a message that fit the receive buffer).
impl TryFrom<client_proto::TransitData> for TransitData {
    type Error = InvalidDataError;

    fn try_from(data: client_proto::TransitData) -> Result<Self, Self::Error> {
        let mut transit_data = TransitData {
            sourced_at_unix_timestamp: data.sourced_at_unix_timestamp,
            simulated_until_unix_timestamp: data.simulated_until_unix_timestamp,
            feed_source: data.feed_source,
            lines: data.lines,
            ..Default::default()
        };
        for stop in data.stops {
            transit_data.push_stop(stop)?;
        }
        for vehicle in data.vehicle_movements {
            transit_data.push_vehicle(vehicle)?;
        }
        for disruption in data.disruptions {
            transit_data.push_disruption(disruption)?;
        }
        transit_data.shrink_to_fit();
        Ok(transit_data)
    }
}
