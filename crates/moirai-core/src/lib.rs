//! Moirai core: three-body physics that looks natural and never falls apart.
//!
//! * [`physics`]: exact Newtonian gravity with a grid-aligned, restartable integrator.
//! * [`events`] (Atropos): recognises ejections, near-collisions and frozen binaries early.
//! * [`steer`] (Lachesis): rewrites the not-yet-shown future with tiny invisible nudges.
//! * [`timeline`] (Clotho): the committed future of a scene, filled ahead of the display.
//! * [`library`] / [`scene`]: stable choreographies, random dances and the director.

pub mod events;
pub mod library;
pub mod physics;
pub mod rng;
pub mod scene;
pub mod steer;
pub mod timeline;

pub use physics::REV;
