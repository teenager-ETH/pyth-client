pub use price_pythnet::*;
#[cfg(test)]
use quickcheck::Arbitrary;
use {
    super::{
        AccountHeader,
        PythAccount,
    },
    crate::c_oracle_header::PC_ACCTYPE_PRICE,
    bytemuck::{
        Pod,
        Zeroable,
    },
    pythnet_sdk::messages::{
        PriceFeedMessage,
        PublisherStakeCapsMessage,
        TwapMessage,
    },
    solana_program::pubkey::Pubkey,
    std::{
        mem::size_of,
        u16,
    },
};

/// Pythnet-specific PriceAccount implementation
mod price_pythnet {
    pub type PriceAccount = PriceAccountPythnet;

    use {
        super::*,
        crate::c_oracle_header::{
            PC_MAX_SEND_LATENCY,
            PC_NUM_COMP_PYTHNET,
            PC_STATUS_TRADING,
        },
        bitflags::bitflags,
    };

    /// Pythnet-only extended price account format. This extension is
    /// an append-only change that adds extra publisher slots and
    /// PriceCumulative for TWAP processing.
    #[repr(C)]
    #[derive(Copy, Clone, Pod, Zeroable)]
    pub struct PriceAccountPythnet {
        pub header:             AccountHeader,
        /// Type of the price account
        pub price_type:         u32,
        /// Exponent for the published prices
        pub exponent:           i32,
        /// Current number of authorized publishers
        pub num_:               u32,
        /// Number of valid quotes for the last aggregation
        pub num_qt_:            u32,
        /// Last slot with a succesful aggregation (status : TRADING)
        pub last_slot_:         u64,
        /// Second to last slot where aggregation was attempted
        pub valid_slot_:        u64,
        /// Ema for price
        pub twap_:              PriceEma,
        /// Ema for confidence
        pub twac_:              PriceEma,
        /// Last time aggregation was attempted
        pub timestamp_:         i64,
        /// Minimum valid publisher quotes for a succesful aggregation
        pub min_pub_:           u8,
        pub message_sent_:      u8,
        /// Configurable max latency in slots between send and receive
        pub max_latency_:       u8,
        /// Various flags
        pub flags:              PriceAccountFlags,
        /// Globally unique price feed index used for publishing.
        /// Limited to 28 bites so that it can be packed together with trading status in a single u32.
        pub feed_index:         u32,
        /// Corresponding product account
        pub product_account:    Pubkey,
        /// Next price account in the list
        pub next_price_account: Pubkey,
        /// Second to last slot where aggregation was succesful (i.e. status : TRADING)
        pub prev_slot_:         u64,
        /// Aggregate price at prev_slot_
        pub prev_price_:        i64,
        /// Confidence interval at prev_slot_
        pub prev_conf_:         u64,
        /// Timestamp of prev_slot_
        pub prev_timestamp_:    i64,
        /// Last attempted aggregate results
        pub agg_:               PriceInfo,
        /// Publishers' price components. NOTE(2023-10-06): On Pythnet, not all
        /// PC_NUM_COMP_PYTHNET slots are used due to stack size
        /// issues in the C code. For iterating over price components,
        /// PC_NUM_COMP must be used.
        pub comp_:              [PriceComponent; PC_NUM_COMP_PYTHNET as usize],
        /// Cumulative sums of aggregative price and confidence used to compute arithmetic moving averages
        pub price_cumulative:   PriceCumulative,
    }

    // Feed index is limited to 28 bites so that it can be packed
    // together with trading status in a single u32.
    pub const MAX_FEED_INDEX: u32 = (1 << 28) - 1;

    bitflags! {
        #[repr(C)]
        #[derive(Copy, Clone, Pod, Zeroable)]
        pub struct PriceAccountFlags: u8 {
            /// If set, the program doesn't do accumulation, but validator does.
            const ACCUMULATOR_V2 = 0b1;
            /// If unset, the program will remove old messages from its message buffer account
            /// and set this flag.
            const MESSAGE_BUFFER_CLEARED = 0b10;
        }
    }

    impl PriceAccountPythnet {
        pub fn as_price_feed_message(&self, key: &Pubkey) -> PriceFeedMessage {
            let (price, conf, publish_time) = if self.agg_.status_ == PC_STATUS_TRADING {
                (self.agg_.price_, self.agg_.conf_, self.timestamp_)
            } else {
                (self.prev_price_, self.prev_conf_, self.prev_timestamp_)
            };

            PriceFeedMessage {
                feed_id: key.to_bytes(),
                price,
                conf,
                exponent: self.exponent,
                publish_time,
                prev_publish_time: self.prev_timestamp_,
                ema_price: self.twap_.val_,
                ema_conf: self.twac_.val_ as u64,
            }
        }
        /// This function gets triggered when there's a succesful aggregation and updates the cumulative sums
        pub fn update_price_cumulative(&mut self) {
            if self.agg_.status_ == PC_STATUS_TRADING {
                self.price_cumulative.update(
                    self.agg_.price_,
                    self.agg_.conf_,
                    self.agg_.pub_slot_.saturating_sub(self.prev_slot_),
                    self.max_latency_,
                ); // pub_slot should always be >= prev_slot, but we protect ourselves against underflow just in case
            }
        }

        pub fn as_twap_message(&self, key: &Pubkey) -> TwapMessage {
            let publish_time = if self.agg_.status_ == PC_STATUS_TRADING {
                self.timestamp_
            } else {
                self.prev_timestamp_
            };

            TwapMessage {
                feed_id: key.to_bytes(),
                cumulative_price: self.price_cumulative.price,
                cumulative_conf: self.price_cumulative.conf,
                num_down_slots: self.price_cumulative.num_down_slots,
                exponent: self.exponent,
                publish_time,
                prev_publish_time: self.prev_timestamp_,
                publish_slot: self.last_slot_,
            }
        }
    }

    impl PythAccount for PriceAccountPythnet {
        const ACCOUNT_TYPE: u32 = PC_ACCTYPE_PRICE;
        const INITIAL_SIZE: u32 = size_of::<PriceAccountPythnet>() as u32;
    }

    // This struct can't overflow since :
    // |sum(price * slotgap)| <= sum(|price * slotgap|) <= max(|price|) * sum(slotgap) <= i64::MAX * * current_slot <= i64::MAX * u64::MAX <= i128::MAX
    // |sum(conf * slotgap)| <= sum(|conf * slotgap|) <= max(|conf|) * sum(slotgap) <= u64::MAX * current_slot <= u64::MAX * u64::MAX <= u128::MAX
    // num_down_slots <= current_slot <= u64::MAX
    /// Contains cumulative sums of aggregative price and confidence used to compute arithmetic moving averages.
    /// Informally the TWAP between time t and time T can be computed as :
    /// `(T.price_cumulative.price - t.price_cumulative.price) / (T.agg_.pub_slot_ - t.agg_.pub_slot_)`
    #[repr(C)]
    #[derive(Copy, Clone, Pod, Zeroable)]
    pub struct PriceCumulative {
        /// Cumulative sum of price * slot_gap
        pub price:          i128,
        /// Cumulative sum of conf * slot_gap
        pub conf:           u128,
        /// Cumulative number of slots where the price wasn't recently updated (within
        /// PC_MAX_SEND_LATENCY slots). This field should be used to calculate the downtime
        /// as a percent of slots between two times `T` and `t` as follows:
        /// `(T.num_down_slots - t.num_down_slots) / (T.agg_.pub_slot_ - t.agg_.pub_slot_)`
        pub num_down_slots: u64,
        /// Padding for alignment
        pub unused:         u64,
    }

    impl PriceCumulative {
        pub fn update(&mut self, price: i64, conf: u64, slot_gap: u64, max_latency: u8) {
            self.price += i128::from(price) * i128::from(slot_gap);
            self.conf += u128::from(conf) * u128::from(slot_gap);
            // Use PC_MAX_SEND_LATENCY if max_latency is 0, otherwise use max_latency
            let latency = if max_latency == 0 {
                u64::from(PC_MAX_SEND_LATENCY)
            } else {
                u64::from(max_latency)
            };
            // This is expected to saturate at 0 most of the time (while the feed is up).
            self.num_down_slots += slot_gap.saturating_sub(latency);
        }
    }
}

#[repr(C)]
#[cfg_attr(test, derive(Debug, PartialEq))]
#[derive(Copy, Clone, Pod, Zeroable)]
pub struct PriceComponent {
    pub pub_:    Pubkey,
    pub agg_:    PriceInfo,
    pub latest_: PriceInfo,
}

#[cfg(test)]
impl Arbitrary for PriceComponent {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        let mut key = [0u8; 32];
        key.iter_mut().for_each(|item| *item = u8::arbitrary(g));
        PriceComponent {
            pub_:    Pubkey::new_from_array(key),
            agg_:    PriceInfo::arbitrary(g),
            latest_: PriceInfo::arbitrary(g),
        }
    }
}

#[repr(C)]
#[cfg_attr(test, derive(Debug, PartialEq))]
#[derive(Copy, Clone, Pod, Zeroable)]
pub struct PriceInfo {
    pub price_:           i64,
    pub conf_:            u64,
    pub status_:          u32,
    pub corp_act_status_: u32,
    pub pub_slot_:        u64,
}

#[cfg(test)]
impl Arbitrary for PriceInfo {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        PriceInfo {
            price_:           i64::arbitrary(g),
            conf_:            u64::arbitrary(g),
            status_:          u32::arbitrary(g),
            corp_act_status_: u32::arbitrary(g),
            pub_slot_:        u64::arbitrary(g),
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
pub struct PriceEma {
    pub val_:   i64,
    pub numer_: i64,
    pub denom_: i64,
}

pub trait PythOracleSerialize {
    fn to_bytes(self) -> Vec<u8>;
}

impl PythOracleSerialize for PriceFeedMessage {
    /// Ideally, this structure should not be aware of the discrminator and the Message enum
    /// should handle serializing the discriminator. However, this method is kept as is in order
    /// to avoid any further increase in the program binary size since we are close to the limit.
    #[allow(unused_assignments)]
    fn to_bytes(self) -> Vec<u8> {
        const MESSAGE_SIZE: usize = 1 + 32 + 8 + 8 + 4 + 8 + 8 + 8 + 8;
        const DISCRIMINATOR: u8 = 0;
        let mut bytes = [0u8; MESSAGE_SIZE];

        let mut i: usize = 0;

        bytes[i..i + 1].clone_from_slice(&[DISCRIMINATOR]);
        i += 1;

        bytes[i..i + 32].clone_from_slice(&self.feed_id[..]);
        i += 32;

        bytes[i..i + 8].clone_from_slice(&self.price.to_be_bytes());
        i += 8;

        bytes[i..i + 8].clone_from_slice(&self.conf.to_be_bytes());
        i += 8;

        bytes[i..i + 4].clone_from_slice(&self.exponent.to_be_bytes());
        i += 4;

        bytes[i..i + 8].clone_from_slice(&self.publish_time.to_be_bytes());
        i += 8;

        bytes[i..i + 8].clone_from_slice(&self.prev_publish_time.to_be_bytes());
        i += 8;

        bytes[i..i + 8].clone_from_slice(&self.ema_price.to_be_bytes());
        i += 8;

        bytes[i..i + 8].clone_from_slice(&self.ema_conf.to_be_bytes());
        i += 8;

        bytes.to_vec()
    }
}

impl PythOracleSerialize for TwapMessage {
    /// Ideally, this structure should not be aware of the discrminator and the Message enum
    /// should handle serializing the discriminator. However, this method is kept as is in order
    /// to avoid any further increase in the program binary size since we are close to the limit.
    #[allow(unused_assignments)]
    fn to_bytes(self) -> Vec<u8> {
        const MESSAGE_SIZE: usize = 1 + 32 + 16 + 16 + 8 + 4 + 8 + 8 + 8;
        const DISCRIMINATOR: u8 = 1;
        let mut bytes = [0u8; MESSAGE_SIZE];

        let mut i: usize = 0;

        bytes[i..i + 1].clone_from_slice(&[DISCRIMINATOR]);
        i += 1;

        bytes[i..i + 32].clone_from_slice(&self.feed_id[..]);
        i += 32;

        bytes[i..i + 16].clone_from_slice(&self.cumulative_price.to_be_bytes());
        i += 16;

        bytes[i..i + 16].clone_from_slice(&self.cumulative_conf.to_be_bytes());
        i += 16;

        bytes[i..i + 8].clone_from_slice(&self.num_down_slots.to_be_bytes());
        i += 8;

        bytes[i..i + 4].clone_from_slice(&self.exponent.to_be_bytes());
        i += 4;

        bytes[i..i + 8].clone_from_slice(&self.publish_time.to_be_bytes());
        i += 8;

        bytes[i..i + 8].clone_from_slice(&self.prev_publish_time.to_be_bytes());
        i += 8;

        bytes[i..i + 8].clone_from_slice(&self.publish_slot.to_be_bytes());
        i += 8;

        bytes.to_vec()
    }
}

impl PythOracleSerialize for PublisherStakeCapsMessage {
    fn to_bytes(self) -> Vec<u8> {
        const DISCRIMINATOR: u8 = 2;
        let mut result = vec![DISCRIMINATOR];
        result.extend_from_slice(&self.publish_time.to_be_bytes());
        result.extend_from_slice(
            &u16::try_from(self.caps.as_ref().len())
                .unwrap_or(u16::MAX)
                .to_be_bytes(),
        );

        for cap in self.caps {
            result.extend_from_slice(&cap.publisher);
            result.extend_from_slice(&cap.cap.to_be_bytes());
        }

        result
    }
}
