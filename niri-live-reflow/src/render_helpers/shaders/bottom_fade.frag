#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

// Bottom fade in texture coordinates: x is where the fade starts (fully opaque above),
// y is where it ends (fully transparent below). Disabled when x >= y.
uniform vec2 fade_y;
// Top fade in texture coordinates: x is fully transparent above, y is fully opaque below.
// Disabled when x >= y.
uniform vec2 fade_top;

void main() {
    vec4 color = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0);
#endif

    float fade = 1.0;
    if (fade_y.x < fade_y.y) {
        fade *= clamp((fade_y.y - v_coords.y) / (fade_y.y - fade_y.x), 0.0, 1.0);
    } else if (v_coords.y >= fade_y.y) {
        fade = 0.0;
    }
    if (fade_top.x < fade_top.y) {
        fade *= clamp((v_coords.y - fade_top.x) / (fade_top.y - fade_top.x), 0.0, 1.0);
    } else if (v_coords.y < fade_top.y) {
        fade = 0.0;
    }

    color = color * fade * alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
