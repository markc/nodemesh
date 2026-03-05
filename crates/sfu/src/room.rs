use std::collections::HashMap;

use tracing::{debug, info};

/// Role of a session within a room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Publisher,
    Subscriber,
}

impl Role {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "publisher" => Some(Role::Publisher),
            "subscriber" => Some(Role::Subscriber),
            _ => None,
        }
    }
}

/// A room containing one optional publisher and N subscribers.
pub struct Room {
    pub name: String,
    /// Session index of the publisher (into the sessions Vec).
    pub publisher: Option<usize>,
    /// Session indices of subscribers.
    pub subscribers: Vec<usize>,
}

/// Registry of active rooms, keyed by room name.
pub struct RoomRegistry {
    pub rooms: HashMap<String, Room>,
}

impl RoomRegistry {
    pub fn new() -> Self {
        Self {
            rooms: HashMap::new(),
        }
    }

    /// Join a session to a room. Creates the room if it doesn't exist.
    pub fn join(&mut self, room_name: &str, idx: usize, role: Role) {
        let room = self.rooms.entry(room_name.to_string()).or_insert_with(|| {
            info!(room = room_name, "room created");
            Room {
                name: room_name.to_string(),
                publisher: None,
                subscribers: Vec::new(),
            }
        });

        match role {
            Role::Publisher => {
                if let Some(old) = room.publisher {
                    info!(room = room_name, old_idx = old, new_idx = idx, "replacing publisher");
                }
                room.publisher = Some(idx);
                debug!(room = room_name, idx, "publisher joined");
            }
            Role::Subscriber => {
                room.subscribers.push(idx);
                debug!(room = room_name, idx, "subscriber joined");
            }
        }
    }

    /// Remove a session from all rooms. Adjusts indices > removed_idx by -1.
    /// Cleans up empty rooms. Call this when a session is removed from the sessions Vec.
    pub fn remove_session(&mut self, removed_idx: usize) {
        let mut empty_rooms = Vec::new();

        for (name, room) in self.rooms.iter_mut() {
            // Remove the session
            if room.publisher == Some(removed_idx) {
                room.publisher = None;
                debug!(room = %name, idx = removed_idx, "publisher removed");
            }
            room.subscribers.retain(|&i| i != removed_idx);

            // Adjust indices > removed_idx
            if let Some(ref mut pub_idx) = room.publisher {
                if *pub_idx > removed_idx {
                    *pub_idx -= 1;
                }
            }
            for sub_idx in room.subscribers.iter_mut() {
                if *sub_idx > removed_idx {
                    *sub_idx -= 1;
                }
            }

            // Track empty rooms
            if room.publisher.is_none() && room.subscribers.is_empty() {
                empty_rooms.push(name.clone());
            }
        }

        for name in empty_rooms {
            self.rooms.remove(&name);
            info!(room = %name, "room destroyed (empty)");
        }
    }

    /// Find the room where the given session index is the publisher.
    pub fn find_publisher_room(&self, idx: usize) -> Option<&Room> {
        self.rooms.values().find(|r| r.publisher == Some(idx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_creates_room() {
        let mut reg = RoomRegistry::new();
        reg.join("test", 0, Role::Publisher);
        assert!(reg.rooms.contains_key("test"));
        assert_eq!(reg.rooms["test"].publisher, Some(0));
    }

    #[test]
    fn join_publisher_and_subscribers() {
        let mut reg = RoomRegistry::new();
        reg.join("test", 0, Role::Publisher);
        reg.join("test", 1, Role::Subscriber);
        reg.join("test", 2, Role::Subscriber);
        assert_eq!(reg.rooms["test"].publisher, Some(0));
        assert_eq!(reg.rooms["test"].subscribers, vec![1, 2]);
    }

    #[test]
    fn remove_publisher_keeps_room() {
        let mut reg = RoomRegistry::new();
        reg.join("test", 0, Role::Publisher);
        reg.join("test", 1, Role::Subscriber);
        reg.remove_session(0);
        // Room still exists (has subscriber), publisher gone, subscriber index adjusted
        assert!(reg.rooms.contains_key("test"));
        assert_eq!(reg.rooms["test"].publisher, None);
        assert_eq!(reg.rooms["test"].subscribers, vec![0]); // 1 -> 0
    }

    #[test]
    fn remove_last_destroys_room() {
        let mut reg = RoomRegistry::new();
        reg.join("test", 0, Role::Publisher);
        reg.remove_session(0);
        assert!(!reg.rooms.contains_key("test"));
    }

    #[test]
    fn remove_subscriber_adjusts_indices() {
        let mut reg = RoomRegistry::new();
        reg.join("test", 0, Role::Publisher);
        reg.join("test", 1, Role::Subscriber);
        reg.join("test", 2, Role::Subscriber);
        reg.join("test", 3, Role::Subscriber);

        // Remove subscriber at index 1
        reg.remove_session(1);
        assert_eq!(reg.rooms["test"].publisher, Some(0));
        assert_eq!(reg.rooms["test"].subscribers, vec![1, 2]); // 2->1, 3->2
    }

    #[test]
    fn find_publisher_room() {
        let mut reg = RoomRegistry::new();
        reg.join("room-a", 0, Role::Publisher);
        reg.join("room-b", 1, Role::Publisher);

        assert_eq!(reg.find_publisher_room(0).unwrap().name, "room-a");
        assert_eq!(reg.find_publisher_room(1).unwrap().name, "room-b");
        assert!(reg.find_publisher_room(2).is_none());
    }
}
