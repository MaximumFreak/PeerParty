#version 450
// The overlay's frame (premultiplied, sRGB-encoded BGRA) into an scRGB swap chain
// (R16G16B16A16_SFLOAT, linear light, 1.0 = 80 nits): decoded to linear light and scaled
// to SDR white, as d3d11.rs's ps_scrgb does. tools/vk_shaders.py compiles it into
// vkshaders.rs.

layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 color;

layout(set = 0, binding = 1) uniform texture2D frame;
layout(set = 0, binding = 2) uniform sampler nearest;

// scRGB 1.0 is 80 nits: SDR white at 200, about where Windows puts SDR content on an HDR
// display by default.
const float SDR_WHITE = 2.5;

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
        vec3 e = clamp(c.rgb / c.a, 0.0, 1.0);
        color = vec4(linear_of(e) * SDR_WHITE * c.a, c.a);
    }
}
