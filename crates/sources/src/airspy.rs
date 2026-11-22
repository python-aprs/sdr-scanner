//! Airspy R2 backend via libairspy (Phase 2).
//!
//! Plan: bindgen over `airspy.h`, start the stream with int16 IQ at 10 MS/s
//! (or raw real samples at 20 MS/s, see PLAN.md 2), copy each callback buffer
//! into the SPSC ring, count overruns, and serve `Source::read` from the ring.
