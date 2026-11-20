#version 450

layout(push_constant) uniform Push {
    mat4 transform;
    vec2 translate;
    vec2 to_ndc;  // 2 / viewport size
    int srgb;
    int labels;  // drawing the display labels: move each by its tracking result
} pc;

// The display labels' boxes and where the GPU found their tooltips this
// frame (track.comp; gpu_track.h).
struct Slot {
    vec2 center;
    vec2 laid;
    vec4 rect;
    int tw2, th2, offset2;
    float norm2;
    int tw4, th4, offset4;
    float norm4;
};
layout(std430, set = 1, binding = 0) readonly buffer Slots {
    int count;
    int srgb_frame;
    int width, height;
    int radius;
    int pad0, pad1, pad2;
    Slot slots[];
};
layout(std430, set = 1, binding = 1) readonly buffer Results { vec4 result[]; };

layout(location = 0) in vec2 in_pos;
layout(location = 1) in vec4 in_color;  // premultiplied, sRGB-encoded
layout(location = 2) in vec2 in_uv;

layout(location = 0) out vec4 color;
layout(location = 1) out vec2 uv;

void main() {
    vec2 pos = in_pos + pc.translate;
    if (pc.labels != 0) {
        // The label this vertex is part of (its box, a little grown for
        // borders' antialiasing), moved to where its tooltip is now.
        for (int i = 0; i < count; i++) {
            vec4 r = slots[i].rect;
            if (pos.x >= r.x - 2.0 && pos.x <= r.z + 2.0 && pos.y >= r.y - 2.0 && pos.y <= r.w + 2.0) {
                if (result[i].w > 0.5) pos += result[i].xy;
                break;
            }
        }
    }
    vec4 p = pc.transform * vec4(pos, 0.0, 1.0);
    // Pixels from the top-left to Vulkan NDC (y points down).
    gl_Position = vec4(p.xy * pc.to_ndc - 1.0, 0.0, 1.0);
    color = in_color;
    uv = in_uv;
}
