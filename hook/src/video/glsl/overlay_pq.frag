#version 450
// The overlay's frame (premultiplied, sRGB-encoded BGRA) into an HDR10 swap chain
// (A2B10G10R10 / A2R10G10B10 in HDR10_ST2084): its colour at SDR white, in BT.2020,
// PQ-encoded (and blended in that encoding, as a plain target blends in sRGB's), as
// d3d11.rs's ps_pq does. tools/vk_shaders.py compiles it into vkshaders.rs.

layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 color;

layout(set = 0, binding = 1) uniform texture2D frame;
layout(set = 0, binding = 2) uniform sampler nearest;

// SDR white, 200 nits, of PQ's 10000.
const float SDR_WHITE = 200.0 / 10000.0;

vec3 linear_of(vec3 e) {
    vec3 low = e / 12.92;
    vec3 high = pow((e + 0.055) / 1.055, vec3(2.4));
    return mix(low, high, step(vec3(0.04045), e));
}

void main() {
    vec4 c = texture(sampler2D(frame, nearest), uv);
    if (c.a <= 0.0) {
        color = c;
    } else {
        vec3 l = linear_of(clamp(c.rgb / c.a, 0.0, 1.0)) * SDR_WHITE;
        // BT.709 to BT.2020 primaries.
        vec3 b = vec3(
            dot(vec3(0.6274040, 0.3292820, 0.0433136), l),
            dot(vec3(0.0690970, 0.9195400, 0.0113612), l),
            dot(vec3(0.0163916, 0.0880132, 0.8955950), l));
        vec3 y = pow(clamp(b, 0.0, 1.0), vec3(0.1593017578125));
        vec3 p = pow((0.8359375 + 18.8515625 * y) / (1.0 + 18.6875 * y), vec3(78.84375));
        color = vec4(p * c.a, c.a);
    }
}
