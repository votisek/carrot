// renderer trait + vulkan/ash implementation.
//
// a frame is an ordered pass list; v1 has one pass (composite textured
// quads). push descriptors required at device creation. damage decides what
// renders, vblank decides when callbacks fire.
//
// NOTE: premult alpha, dual image views, per-window scissor, geometry offset
// silently break real clients when missing.

pub mod loader;
pub mod renderer;
pub mod shaders;
pub mod vulkan;
