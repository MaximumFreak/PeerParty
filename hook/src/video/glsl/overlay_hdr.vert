#version 450
// The overlay's quad for HDR swap chains (vulkan.rs), with the same vertices and push
// constants as the plain one: pixels to clip space, the frame's UV passed on.
// tools/vk_shaders.py compiles it into vkshaders.rs.

layout(location = 0) in vec2 pos;
layout(location = 1) in vec2 tex_uv;

layout(push_constant) uniform Push {
    vec2 scale;
    vec2 translate;
} pc;

layout(location = 0) out vec2 uv;

void main() {
    uv = tex_uv;
    gl_Position = vec4(pos * pc.scale + pc.translate, 0.0, 1.0);
}
