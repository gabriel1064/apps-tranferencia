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

uniform float niri_scale;

uniform vec2 geo_size;
uniform vec4 corner_radius;
uniform mat3 input_to_geo;
uniform vec2 cutout_loc;
uniform vec2 cutout_size;
uniform float cutout_radius;

float niri_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius);
vec4 postprocess(vec4 color);

void main() {
    vec3 coords_geo = input_to_geo * vec3(v_coords, 1.0);

    // Sample the texture.
    vec4 color = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0);
#endif

    color = postprocess(color);

    if (coords_geo.x < 0.0 || 1.0 < coords_geo.x || coords_geo.y < 0.0 || 1.0 < coords_geo.y) {
        // Clip outside geometry.
        color = vec4(0.0);
    } else {
        // Apply corner rounding inside geometry.
        vec2 p = coords_geo.xy * geo_size;
        color = color * niri_rounding_alpha(p, geo_size, corner_radius);

        // Punch a rounded hole (Super chrome): wallpaper shows through.
        if (cutout_size.x > 0.5 && cutout_size.y > 0.5) {
            vec2 c = p - cutout_loc;
            if (c.x >= 0.0 && c.y >= 0.0 && c.x <= cutout_size.x && c.y <= cutout_size.y) {
                float inside_hole = niri_rounding_alpha(
                    c,
                    cutout_size,
                    vec4(0.0, 0.0, 0.0, cutout_radius)
                );
                color = color * (1.0 - inside_hole);
            }

            float R = cutout_radius;
            if (R > 0.5) {
                // Convex rounding at the top of the vertical cut (window TR of the left piece).
                vec2 tr1 = vec2(cutout_loc.x - R, R);
                if (p.x >= cutout_loc.x - R && p.x <= cutout_loc.x && p.y >= 0.0 && p.y <= R) {
                    float dist = distance(p, tr1);
                    float t = clamp((dist - R) * niri_scale + 0.5, 0.0, 1.0);
                    color = color * (1.0 - t * t * (3.0 - 2.0 * t));
                }

                // Convex rounding where the horizontal cut meets the right edge.
                vec2 cut_br = cutout_loc + cutout_size;
                vec2 tr2 = vec2(geo_size.x - R, cut_br.y + R);
                if (p.x >= geo_size.x - R && p.y >= cut_br.y && p.y <= cut_br.y + R) {
                    float dist = distance(p, tr2);
                    float t = clamp((dist - R) * niri_scale + 0.5, 0.0, 1.0);
                    color = color * (1.0 - t * t * (3.0 - 2.0 * t));
                }
            }
        }
    }

    // Apply final alpha and tint.
    color = color * alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
