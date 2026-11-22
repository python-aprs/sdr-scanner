//! SDRplay RSP1A backend via the SDRplay API 3.x (Phase 2).
//!
//! Plan: bindgen over `sdrplay_api.h` (the API is not redistributable, so CI
//! builds without this feature), open the device through `sdrplay_apiService`,
//! zero-IF at 6–8 MS/s for the full 2 m band, interleave the separate `xi`/`xq`
//! callback arrays into the SPSC ring, and serve `Source::read` from the ring.
