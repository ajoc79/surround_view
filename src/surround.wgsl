struct View {
    orientation: vec4f,
    size: vec4f,
}

@group(0) @binding(0) var<uniform> values: array<vec4f, 29>;
@group(0) @binding(1) var<uniform> projection: View;
@group(0) @binding(2) var mosaic: texture_2d<f32>;
@group(0) @binding(3) var linear_sampler: sampler;

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4f {
    let xy = array<vec2f, 3>(vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0));
    return vec4f(xy[index], 0.0, 1.0);
}

fn bowl_hit(ray: vec3f) -> vec3f {
    let eye = 1.35;
    let speed = length(ray.xy / values[0].xy);
    if speed < 1e-6 {
        if ray.z < 0.0 { return vec3f(0.0, 0.0, 1.0); }
        return vec3f(0.0);
    }
    let slope = ray.z / speed;
    if eye + slope > 2.0 { return vec3f(0.0); }
    if eye + 0.7 * slope <= 0.0 {
        return vec3f(ray.xy * (-eye / ray.z), 1.0);
    }

    // The curved wall is z = 2 * u² where u = (radius - 0.7) / 0.3.
    let wall_slope = 0.3 * slope;
    let wall_start = eye + 0.7 * slope;
    let u = (wall_slope + sqrt(wall_slope * wall_slope + 8.0 * wall_start)) * 0.25;
    return vec3f(ray.xy * ((0.7 + 0.3 * u) / speed), 1.0);
}

fn view_ray(uv: vec2f) -> vec3f {
    let yaw = projection.orientation.y;
    let pitch = projection.orientation.z;
    if projection.orientation.x == 2.0 {
        let azimuth = (uv.x - 0.5) * 6.28318531;
        let elevation = (0.5 - uv.y) * 3.14159265;
        return vec3f(-sin(azimuth) * cos(elevation), cos(azimuth) * cos(elevation), sin(elevation));
    }
    let forward = vec3f(-sin(yaw) * cos(pitch), cos(yaw) * cos(pitch), sin(pitch));
    let right = vec3f(-cos(yaw), -sin(yaw), 0.0);
    let up = cross(right, forward) * -1.0;
    let aspect = projection.size.x / projection.size.y;
    let scale = tan(projection.orientation.w * 0.5);
    return normalize(forward + scale * ((2.0 * uv.x - 1.0) * aspect * right + (1.0 - 2.0 * uv.y) * up));
}

fn compose(xy: vec2f) -> vec4f {
    if all(abs(xy) < values[0].zw) { return values[4]; }

    let native = values[1].xy;
    let border = values[1].z;
    let mosaic_size = vec2f(textureDimensions(mosaic));
    let tile_size = mosaic_size * 0.5;
    var sum = vec3f(0.0);
    var weights = 0.0;
    for (var camera = 0u; camera < 4u; camera++) {
        let base = 5u + 6u * camera;
        let intrinsics = values[base];
        let lens = values[base + 1u];
        let point = vec3f(xy, 1.0);
        let ray = vec3f(dot(values[base+2u].xyz, point), dot(values[base+3u].xyz, point), dot(values[base+4u].xyz, point));
        let magnitude = length(ray);
        let denominator = ray.z + lens.x * magnitude;
        if denominator <= 1e-9 || (lens.x > 1.0 && 1.0 + lens.x * ray.z / max(magnitude, 1e-9) <= 0.0) { continue; }
        let coord = ray.xy / denominator;
        let r2 = dot(coord, coord);
        if r2 >= lens.w { continue; }
        let radial = 1.0 + lens.y * r2 + lens.z * r2 * r2;
        let uv = intrinsics.xy * coord * radial + intrinsics.zw;
        let tile = uv * tile_size / native;
        if any(uv < vec2f(border)) || any(uv >= native - border)
            || any(tile < vec2f(0.0)) || any(tile > tile_size - 1.0) { continue; }

        let origin = vec2f(f32(camera % 2u), f32(camera / 2u)) * tile_size;
        let sample_rgb = textureSampleLevel(mosaic, linear_sampler, (origin + tile + 0.5) / mosaic_size, 0.0).rgb;
        if max(max(sample_rgb.r, sample_rgb.g), sample_rgb.b) <= values[1].w { continue; }
        let blend = values[base+5u];
        let delta = xy - blend.xy;
        let cosine = dot(delta, blend.zw) / max(length(delta), 1e-9);
        let edge_distance = min(uv, native - 1.0 - uv);
        let fade = clamp(min(edge_distance.x, edge_distance.y) / values[2].y, 0.0, 1.0);
        let weight = pow(max(cosine, 0.0), values[2].x) * fade;
        sum += sample_rgb * weight;
        weights += weight;
    }
    if weights <= values[2].z { return values[3]; }
    return vec4f(sum / weights, 1.0);
}

@fragment
fn fragment(@builtin(position) pixel: vec4f) -> @location(0) vec4f {
    if projection.orientation.x == 0.0 {
        let uv = (pixel.xy - 0.5) / (projection.size.xy - 1.0);
        return compose((1.0 - 2.0 * uv) * values[0].xy);
    }
    let hit = bowl_hit(view_ray(pixel.xy / projection.size.xy));
    if hit.z == 0.0 { return values[3]; }
    return compose(hit.xy);
}
