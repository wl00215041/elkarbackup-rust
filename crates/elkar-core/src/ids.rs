//! Strongly typed identifiers, so a `JobId` can never be passed where a
//! `ClientId` is expected.

macro_rules! define_id {
    ($($name:ident),* $(,)?) => {$(
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl $name {
            /// Zero padded form used in on-disk paths (`0007`).
            pub fn padded(self) -> String {
                format!("{:04}", self.0)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl From<i64> for $name {
            fn from(value: i64) -> Self {
                Self(value)
            }
        }
    )*};
}

define_id!(UserId, ClientId, JobId, PolicyId, ScriptId, LocationId, QueueId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padded_uses_four_digits() {
        assert_eq!(ClientId(7).padded(), "0007");
        assert_eq!(JobId(12345).padded(), "12345");
    }
}
