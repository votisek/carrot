// hand-builds carrot's spir-v modules and regenerates
// src/render/shaders.rs. run from the repo (RUSTFLAGS outranks and sheds
// the compositor's static-link target rustflags, which cargo would
// otherwise apply to this host tool too):
//   RUSTFLAGS=" " cargo run --manifest-path tools/gen-shaders/Cargo.toml
//
// no shader language anywhere - the modules are assembled instruction
// by instruction below. the freshness test in the generated file pins
// this file's hash, so editing here without rerunning fails cargo test.
//
// both modules share one layout convention:
//   vertex-less TRIANGLE_STRIP quad, corner = (idx & 1, idx >> 1)
//   push constants hold final vulkan NDC rects - no transforms on gpu
//
// fill push block (32 bytes):  vec2 dst_pos @0, vec2 dst_size @8,
//                              vec4 color @16
// tex push block (36 bytes):   vec2 dst_pos @0, vec2 dst_size @8,
//                              vec2 uv_pos @16, vec2 uv_size @24,
//                              float mul @32
// tex sampler: combined image sampler, set 0 binding 0

use rspirv::binary::Assemble;
use rspirv::dr::{Builder, Operand};
use rspirv::spirv::{
    AddressingModel, BuiltIn, Capability, Decoration, Dim, ExecutionMode, ExecutionModel,
    FunctionControl, ImageFormat, MemoryModel, StorageClass, Word,
};

// GLSL.std.450 opcodes (the extended set is frozen; numbers are spec)
#[derive(Copy, Clone)]
#[repr(u32)]
enum GLOp {
    FAbs = 4,
    Pow = 26,
    FMax = 40,
    FClamp = 43,
    FMix = 46,
    SmoothStep = 49,
    Length = 66,
}
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::PathBuf;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn words_hash(words: &[u32]) -> String {
    let mut h = Sha256::new();
    for w in words {
        h.update(w.to_le_bytes());
    }
    hex(&h.finalize())
}

// the type/constant ids every function body needs
struct Common {
    void: Word,
    fn_void: Word,
    f32t: Word,
    i32t: Word,
    vec2: Word,
    vec4: Word,
    c_i32_0: Word,
    c_i32_1: Word,
    c_i32_2: Word,
    c_f32_0: Word,
    c_f32_1: Word,
    ptr_in_i32: Word,
    ptr_out_vec4: Word,
    ptr_pc_vec2: Word,
}

fn common(b: &mut Builder) -> Common {
    let void = b.type_void();
    let fn_void = b.type_function(void, vec![]);
    let f32t = b.type_float(32, None);
    let i32t = b.type_int(32, 1);
    let vec2 = b.type_vector(f32t, 2);
    let vec4 = b.type_vector(f32t, 4);
    Common {
        void,
        fn_void,
        f32t,
        i32t,
        vec2,
        vec4,
        c_i32_0: b.constant_bit32(i32t, 0),
        c_i32_1: b.constant_bit32(i32t, 1),
        c_i32_2: b.constant_bit32(i32t, 2),
        c_f32_0: b.constant_bit32(f32t, 0.0f32.to_bits()),
        c_f32_1: b.constant_bit32(f32t, 1.0f32.to_bits()),
        ptr_in_i32: b.type_pointer(None, StorageClass::Input, i32t),
        ptr_out_vec4: b.type_pointer(None, StorageClass::Output, vec4),
        ptr_pc_vec2: b.type_pointer(None, StorageClass::PushConstant, vec2),
    }
}

// gl_VertexIndex -> corner (0,0)(1,0)(0,1)(1,1) as vec2, then
// rect_pos + corner * rect_size. returns the resulting vec2 id.
// emits into the currently open block.
fn corner_math(
    b: &mut Builder,
    c: &Common,
    vertex_index_var: Word,
    pc_var: Word,
    pos_member: Word,
    size_member: Word,
) -> Word {
    let idx = b.load(c.i32t, None, vertex_index_var, None, vec![]).unwrap();
    let x_i = b.bitwise_and(c.i32t, None, idx, c.c_i32_1).unwrap();
    let y_i = b
        .shift_right_arithmetic(c.i32t, None, idx, c.c_i32_1)
        .unwrap();
    let x = b.convert_s_to_f(c.f32t, None, x_i).unwrap();
    let y = b.convert_s_to_f(c.f32t, None, y_i).unwrap();
    let corner = b.composite_construct(c.vec2, None, vec![x, y]).unwrap();
    let pos_ptr = b
        .access_chain(c.ptr_pc_vec2, None, pc_var, vec![pos_member])
        .unwrap();
    let pos = b.load(c.vec2, None, pos_ptr, None, vec![]).unwrap();
    let size_ptr = b
        .access_chain(c.ptr_pc_vec2, None, pc_var, vec![size_member])
        .unwrap();
    let size = b.load(c.vec2, None, size_ptr, None, vec![]).unwrap();
    let scaled = b.f_mul(c.vec2, None, corner, size).unwrap();
    b.f_add(c.vec2, None, pos, scaled).unwrap()
}

// vec2 -> vec4(v, 0, 1), stored to an Output vec4
fn store_position(b: &mut Builder, c: &Common, pos_var: Word, v: Word) {
    let v4 = b
        .composite_construct(c.vec4, None, vec![v, c.c_f32_0, c.c_f32_1])
        .unwrap();
    b.store(pos_var, v4, None, vec![]).unwrap();
}

// one GLSL.std.450 call; operands are ids
fn glsl(b: &mut Builder, set: Word, ty: Word, op: GLOp, args: &[Word]) -> Word {
    let ops: Vec<Operand> = args.iter().map(|a| Operand::IdRef(*a)).collect();
    b.ext_inst(ty, None, set, op as u32, ops).unwrap()
}

// circular rounded-rect coverage at a pixel: 1 inside, smooth aa edge.
// frag/geo_pos/geo_size in pixels, radius > aa assumed (op-side floor)
#[allow(clippy::too_many_arguments)]
fn rounding_alpha(
    b: &mut Builder,
    c: &Common,
    set: Word,
    frag: Word,
    geo_pos: Word,
    geo_size: Word,
    radius: Word,
    aa: Word,
) -> Word {
    let c_half = b.constant_bit32(c.f32t, 0.5f32.to_bits());
    let zero2 = b.constant_composite(c.vec2, vec![c.c_f32_0, c.c_f32_0]);
    let half = b.vector_times_scalar(c.vec2, None, geo_size, c_half).unwrap();
    let center = b.f_add(c.vec2, None, geo_pos, half).unwrap();
    let rel = b.f_sub(c.vec2, None, frag, center).unwrap();
    let absd = glsl(b, set, c.vec2, GLOp::FAbs, &[rel]);
    let rvec = b.composite_construct(c.vec2, None, vec![radius, radius]).unwrap();
    let inner = b.f_sub(c.vec2, None, half, rvec).unwrap();
    let p = b.f_sub(c.vec2, None, absd, inner).unwrap();
    let q = glsl(b, set, c.vec2, GLOp::FMax, &[p, zero2]);
    let dist = glsl(b, set, c.f32t, GLOp::Length, &[q]);
    let lo = b.f_sub(c.f32t, None, radius, aa).unwrap();
    let hi = b.f_add(c.f32t, None, radius, aa).unwrap();
    let sm = glsl(b, set, c.f32t, GLOp::SmoothStep, &[lo, hi, dist]);
    b.f_sub(c.f32t, None, c.c_f32_1, sm).unwrap()
}

fn build_fill() -> Vec<u32> {
    let mut b = Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let c = common(&mut b);

    // push block { vec2 dst_pos; vec2 dst_size; vec4 color; }
    let pc_struct = b.type_struct(vec![c.vec2, c.vec2, c.vec4]);
    b.decorate(pc_struct, Decoration::Block, vec![]);
    b.member_decorate(pc_struct, 0, Decoration::Offset, vec![Operand::LiteralBit32(0)]);
    b.member_decorate(pc_struct, 1, Decoration::Offset, vec![Operand::LiteralBit32(8)]);
    b.member_decorate(pc_struct, 2, Decoration::Offset, vec![Operand::LiteralBit32(16)]);
    let ptr_pc = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let pc_var = b.variable(ptr_pc, None, StorageClass::PushConstant, None);
    let ptr_pc_vec4 = b.type_pointer(None, StorageClass::PushConstant, c.vec4);

    // vertex globals
    let vidx = b.variable(c.ptr_in_i32, None, StorageClass::Input, None);
    b.decorate(
        vidx,
        Decoration::BuiltIn,
        vec![Operand::BuiltIn(BuiltIn::VertexIndex)],
    );
    let gl_pos = b.variable(c.ptr_out_vec4, None, StorageClass::Output, None);
    b.decorate(
        gl_pos,
        Decoration::BuiltIn,
        vec![Operand::BuiltIn(BuiltIn::Position)],
    );

    // fragment globals
    let out_color = b.variable(c.ptr_out_vec4, None, StorageClass::Output, None);
    b.decorate(out_color, Decoration::Location, vec![Operand::LiteralBit32(0)]);

    // vs_main
    let vs = b
        .begin_function(c.void, None, FunctionControl::NONE, c.fn_void)
        .unwrap();
    b.begin_block(None).unwrap();
    let ndc = corner_math(&mut b, &c, vidx, pc_var, c.c_i32_0, c.c_i32_1);
    store_position(&mut b, &c, gl_pos, ndc);
    b.ret().unwrap();
    b.end_function().unwrap();

    // fs_main
    let fs = b
        .begin_function(c.void, None, FunctionControl::NONE, c.fn_void)
        .unwrap();
    b.begin_block(None).unwrap();
    let color_ptr = b
        .access_chain(ptr_pc_vec4, None, pc_var, vec![c.c_i32_2])
        .unwrap();
    let color = b.load(c.vec4, None, color_ptr, None, vec![]).unwrap();
    b.store(out_color, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    b.entry_point(ExecutionModel::Vertex, vs, "vs_main", vec![vidx, gl_pos]);
    b.entry_point(ExecutionModel::Fragment, fs, "fs_main", vec![out_color]);
    b.execution_mode(fs, ExecutionMode::OriginUpperLeft, vec![]);

    b.module().assemble()
}

fn build_tex() -> Vec<u32> {
    let mut b = Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let c = common(&mut b);

    // push block { vec2 dst_pos; vec2 dst_size; vec2 uv_pos;
    //              vec2 uv_size; float mul; }
    let pc_struct = b.type_struct(vec![c.vec2, c.vec2, c.vec2, c.vec2, c.f32t]);
    b.decorate(pc_struct, Decoration::Block, vec![]);
    for (i, off) in [0u32, 8, 16, 24, 32].iter().enumerate() {
        b.member_decorate(
            pc_struct,
            i as u32,
            Decoration::Offset,
            vec![Operand::LiteralBit32(*off)],
        );
    }
    let ptr_pc = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let pc_var = b.variable(ptr_pc, None, StorageClass::PushConstant, None);
    let ptr_pc_f32 = b.type_pointer(None, StorageClass::PushConstant, c.f32t);
    let c_i32_3 = b.constant_bit32(c.i32t, 3);
    let c_i32_4 = b.constant_bit32(c.i32t, 4);

    // combined image sampler, set 0 binding 0
    let image = b.type_image(
        c.f32t,
        Dim::Dim2D,
        0,
        0,
        0,
        1,
        ImageFormat::Unknown,
        None,
    );
    let sampled = b.type_sampled_image(image);
    let ptr_uc = b.type_pointer(None, StorageClass::UniformConstant, sampled);
    let tex = b.variable(ptr_uc, None, StorageClass::UniformConstant, None);
    b.decorate(tex, Decoration::DescriptorSet, vec![Operand::LiteralBit32(0)]);
    b.decorate(tex, Decoration::Binding, vec![Operand::LiteralBit32(0)]);

    // vertex globals
    let vidx = b.variable(c.ptr_in_i32, None, StorageClass::Input, None);
    b.decorate(
        vidx,
        Decoration::BuiltIn,
        vec![Operand::BuiltIn(BuiltIn::VertexIndex)],
    );
    let gl_pos = b.variable(c.ptr_out_vec4, None, StorageClass::Output, None);
    b.decorate(
        gl_pos,
        Decoration::BuiltIn,
        vec![Operand::BuiltIn(BuiltIn::Position)],
    );
    let ptr_out_vec2 = b.type_pointer(None, StorageClass::Output, c.vec2);
    let uv_out = b.variable(ptr_out_vec2, None, StorageClass::Output, None);
    b.decorate(uv_out, Decoration::Location, vec![Operand::LiteralBit32(0)]);

    // fragment globals
    let ptr_in_vec2 = b.type_pointer(None, StorageClass::Input, c.vec2);
    let uv_in = b.variable(ptr_in_vec2, None, StorageClass::Input, None);
    b.decorate(uv_in, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let out_color = b.variable(c.ptr_out_vec4, None, StorageClass::Output, None);
    b.decorate(out_color, Decoration::Location, vec![Operand::LiteralBit32(0)]);

    // vs_main
    let vs = b
        .begin_function(c.void, None, FunctionControl::NONE, c.fn_void)
        .unwrap();
    b.begin_block(None).unwrap();
    let ndc = corner_math(&mut b, &c, vidx, pc_var, c.c_i32_0, c.c_i32_1);
    store_position(&mut b, &c, gl_pos, ndc);
    let uv = corner_math(&mut b, &c, vidx, pc_var, c.c_i32_2, c_i32_3);
    b.store(uv_out, uv, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    // fs_main
    let fs = b
        .begin_function(c.void, None, FunctionControl::NONE, c.fn_void)
        .unwrap();
    b.begin_block(None).unwrap();
    let si = b.load(sampled, None, tex, None, vec![]).unwrap();
    let uv = b.load(c.vec2, None, uv_in, None, vec![]).unwrap();
    let texel = b
        .image_sample_implicit_lod(c.vec4, None, si, uv, None, vec![])
        .unwrap();
    let mul_ptr = b
        .access_chain(ptr_pc_f32, None, pc_var, vec![c_i32_4])
        .unwrap();
    let mul = b.load(c.f32t, None, mul_ptr, None, vec![]).unwrap();
    let scaled = b.vector_times_scalar(c.vec4, None, texel, mul).unwrap();
    b.store(out_color, scaled, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    b.entry_point(
        ExecutionModel::Vertex,
        vs,
        "vs_main",
        vec![vidx, gl_pos, uv_out],
    );
    b.entry_point(
        ExecutionModel::Fragment,
        fs,
        "fs_main",
        vec![uv_in, out_color],
    );
    b.execution_mode(fs, ExecutionMode::OriginUpperLeft, vec![]);

    b.module().assemble()
}

// texr push block (64 bytes): tex's five members, then
//   vec2 geo_pos @40, vec2 geo_size @48, float radius @56, float aa @60
// geo is the window geometry in output-local pixels; the fragment side
// clips the sample to its rounded rect via FragCoord
fn build_texr() -> Vec<u32> {
    let mut b = Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    let set = b.ext_inst_import("GLSL.std.450");
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let c = common(&mut b);

    let pc_struct = b.type_struct(vec![
        c.vec2, c.vec2, c.vec2, c.vec2, c.f32t, c.vec2, c.vec2, c.f32t, c.f32t,
    ]);
    b.decorate(pc_struct, Decoration::Block, vec![]);
    for (i, off) in [0u32, 8, 16, 24, 32, 40, 48, 56, 60].iter().enumerate() {
        b.member_decorate(
            pc_struct,
            i as u32,
            Decoration::Offset,
            vec![Operand::LiteralBit32(*off)],
        );
    }
    let ptr_pc = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let pc_var = b.variable(ptr_pc, None, StorageClass::PushConstant, None);
    let ptr_pc_f32 = b.type_pointer(None, StorageClass::PushConstant, c.f32t);
    let c_i32_3 = b.constant_bit32(c.i32t, 3);
    let c_i32_4 = b.constant_bit32(c.i32t, 4);
    let c_i32_5 = b.constant_bit32(c.i32t, 5);
    let c_i32_6 = b.constant_bit32(c.i32t, 6);
    let c_i32_7 = b.constant_bit32(c.i32t, 7);
    let c_i32_8 = b.constant_bit32(c.i32t, 8);

    let image = b.type_image(c.f32t, Dim::Dim2D, 0, 0, 0, 1, ImageFormat::Unknown, None);
    let sampled = b.type_sampled_image(image);
    let ptr_uc = b.type_pointer(None, StorageClass::UniformConstant, sampled);
    let tex = b.variable(ptr_uc, None, StorageClass::UniformConstant, None);
    b.decorate(tex, Decoration::DescriptorSet, vec![Operand::LiteralBit32(0)]);
    b.decorate(tex, Decoration::Binding, vec![Operand::LiteralBit32(0)]);

    // vertex globals
    let vidx = b.variable(c.ptr_in_i32, None, StorageClass::Input, None);
    b.decorate(vidx, Decoration::BuiltIn, vec![Operand::BuiltIn(BuiltIn::VertexIndex)]);
    let gl_pos = b.variable(c.ptr_out_vec4, None, StorageClass::Output, None);
    b.decorate(gl_pos, Decoration::BuiltIn, vec![Operand::BuiltIn(BuiltIn::Position)]);
    let ptr_out_vec2 = b.type_pointer(None, StorageClass::Output, c.vec2);
    let uv_out = b.variable(ptr_out_vec2, None, StorageClass::Output, None);
    b.decorate(uv_out, Decoration::Location, vec![Operand::LiteralBit32(0)]);

    // fragment globals
    let ptr_in_vec2 = b.type_pointer(None, StorageClass::Input, c.vec2);
    let uv_in = b.variable(ptr_in_vec2, None, StorageClass::Input, None);
    b.decorate(uv_in, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let ptr_in_vec4 = b.type_pointer(None, StorageClass::Input, c.vec4);
    let frag_coord = b.variable(ptr_in_vec4, None, StorageClass::Input, None);
    b.decorate(
        frag_coord,
        Decoration::BuiltIn,
        vec![Operand::BuiltIn(BuiltIn::FragCoord)],
    );
    let out_color = b.variable(c.ptr_out_vec4, None, StorageClass::Output, None);
    b.decorate(out_color, Decoration::Location, vec![Operand::LiteralBit32(0)]);

    // vs_main - identical to tex
    let vs = b
        .begin_function(c.void, None, FunctionControl::NONE, c.fn_void)
        .unwrap();
    b.begin_block(None).unwrap();
    let ndc = corner_math(&mut b, &c, vidx, pc_var, c.c_i32_0, c.c_i32_1);
    store_position(&mut b, &c, gl_pos, ndc);
    let uv = corner_math(&mut b, &c, vidx, pc_var, c.c_i32_2, c_i32_3);
    b.store(uv_out, uv, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    // fs_main - tex sample scaled by mul and the rounded-rect coverage
    let fs = b
        .begin_function(c.void, None, FunctionControl::NONE, c.fn_void)
        .unwrap();
    b.begin_block(None).unwrap();
    let si = b.load(sampled, None, tex, None, vec![]).unwrap();
    let uv = b.load(c.vec2, None, uv_in, None, vec![]).unwrap();
    let texel = b
        .image_sample_implicit_lod(c.vec4, None, si, uv, None, vec![])
        .unwrap();
    let mul_ptr = b.access_chain(ptr_pc_f32, None, pc_var, vec![c_i32_4]).unwrap();
    let mul = b.load(c.f32t, None, mul_ptr, None, vec![]).unwrap();
    let f4 = b.load(c.vec4, None, frag_coord, None, vec![]).unwrap();
    let frag = b
        .vector_shuffle(c.vec2, None, f4, f4, vec![0, 1])
        .unwrap();
    let gp_ptr = b.access_chain(c.ptr_pc_vec2, None, pc_var, vec![c_i32_5]).unwrap();
    let geo_pos = b.load(c.vec2, None, gp_ptr, None, vec![]).unwrap();
    let gs_ptr = b.access_chain(c.ptr_pc_vec2, None, pc_var, vec![c_i32_6]).unwrap();
    let geo_size = b.load(c.vec2, None, gs_ptr, None, vec![]).unwrap();
    let r_ptr = b.access_chain(ptr_pc_f32, None, pc_var, vec![c_i32_7]).unwrap();
    let radius = b.load(c.f32t, None, r_ptr, None, vec![]).unwrap();
    let aa_ptr = b.access_chain(ptr_pc_f32, None, pc_var, vec![c_i32_8]).unwrap();
    let aa = b.load(c.f32t, None, aa_ptr, None, vec![]).unwrap();
    let cover = rounding_alpha(&mut b, &c, set, frag, geo_pos, geo_size, radius, aa);
    let k = b.f_mul(c.f32t, None, mul, cover).unwrap();
    let scaled = b.vector_times_scalar(c.vec4, None, texel, k).unwrap();
    b.store(out_color, scaled, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    b.entry_point(ExecutionModel::Vertex, vs, "vs_main", vec![vidx, gl_pos, uv_out]);
    b.entry_point(
        ExecutionModel::Fragment,
        fs,
        "fs_main",
        vec![uv_in, frag_coord, out_color],
    );
    b.execution_mode(fs, ExecutionMode::OriginUpperLeft, vec![]);

    b.module().assemble()
}

fn emit_const(out: &mut String, name: &str, words: &[u32]) {
    writeln!(out, "pub const {name}: &[u32] = &[").unwrap();
    for chunk in words.chunks(8) {
        let line: Vec<String> = chunk.iter().map(|w| format!("{w:#010x},")).collect();
        writeln!(out, "    {}", line.join(" ")).unwrap();
    }
    writeln!(out, "];\n").unwrap();
}

fn main() {
    let tool_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let render_dir = tool_dir.join("../../src/render");

    let fill = build_fill();
    let tex = build_tex();
    let texr = build_texr();

    let own_src = std::fs::read_to_string(tool_dir.join("src/main.rs")).unwrap();
    let gen_hash = hex(&Sha256::digest(own_src.as_bytes()));

    let mut out = String::from(
        "// generated by tools/gen-shaders - DO NOT EDIT. the spir-v is\n\
         // hand-assembled in that crate (no shader language exists in this\n\
         // repo); to regenerate:\n\
         //   cargo run --manifest-path tools/gen-shaders/Cargo.toml\n\
         // the tests below fail if the generator and these words drift.\n\n",
    );
    emit_const(&mut out, "FILL", &fill);
    emit_const(&mut out, "TEX", &tex);
    emit_const(&mut out, "TEXR", &texr);

    writeln!(
        out,
        r#"#[cfg(test)]
mod tests {{
    use sha2::{{Digest, Sha256}};

    // pins tools/gen-shaders/src/main.rs as of the last regeneration
    const GEN_SRC_HASH: &str = "{gen_hash}";
    const FILL_HASH: &str = "{fill_hash}";
    const TEX_HASH: &str = "{tex_hash}";
    const TEXR_HASH: &str = "{texr_hash}";
    const REGEN: &str =
        "shaders out of date - rerun: cargo run --manifest-path tools/gen-shaders/Cargo.toml";

    fn hex(b: &[u8]) -> String {{
        b.iter().map(|x| format!("{{x:02x}}")).collect()
    }}

    fn words_hash(words: &[u32]) -> String {{
        let mut h = Sha256::new();
        for w in words {{
            h.update(w.to_le_bytes());
        }}
        hex(&h.finalize())
    }}

    #[test]
    fn generator_matches_committed_words() {{
        let gen_src = include_str!("../../tools/gen-shaders/src/main.rs");
        assert_eq!(GEN_SRC_HASH, hex(&Sha256::digest(gen_src.as_bytes())), "{{REGEN}}");
        assert_eq!(FILL_HASH, words_hash(super::FILL), "{{REGEN}}");
        assert_eq!(TEX_HASH, words_hash(super::TEX), "{{REGEN}}");
        assert_eq!(TEXR_HASH, words_hash(super::TEXR), "{{REGEN}}");
    }}

    #[test]
    fn shader_words_are_spirv() {{
        assert_eq!(super::FILL[0], 0x0723_0203);
        assert_eq!(super::TEX[0], 0x0723_0203);
        assert_eq!(super::TEXR[0], 0x0723_0203);
    }}
}}"#,
        gen_hash = gen_hash,
        fill_hash = words_hash(&fill),
        tex_hash = words_hash(&tex),
        texr_hash = words_hash(&texr),
    )
    .unwrap();

    std::fs::write(render_dir.join("shaders.rs"), out).unwrap();
    println!(
        "wrote shaders.rs (fill {} words, tex {} words, texr {} words)",
        fill.len(),
        tex.len(),
        texr.len()
    );
}
