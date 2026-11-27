#include "msdfgen.h"

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <vector>

struct Edge {
    double points[4][2];
    std::int32_t order;
};

extern "C" int sg_msdfgen_generate(
    const Edge *edges, const std::size_t *contour_ends, std::size_t contour_count,
    std::int32_t width, std::int32_t height, double spread,
    std::uint64_t seed, std::uint8_t *pixels
) noexcept {
    try {
        using namespace msdfgen;
        Shape shape;
        shape.setYAxisOrientation(Y_DOWNWARD);
        std::size_t edge_index = 0;
        for (std::size_t i = 0; i < contour_count; ++i) {
            Contour &contour = shape.addContour();
            for (; edge_index < contour_ends[i]; ++edge_index) {
                const Edge &edge = edges[edge_index];
                const auto point = [&](int j) {
                    return Point2(edge.points[j][0], edge.points[j][1]);
                };
                switch (edge.order) {
                    case 1: contour.addEdge(EdgeHolder(point(0), point(1))); break;
                    case 2: contour.addEdge(EdgeHolder(point(0), point(1), point(2))); break;
                    case 3: contour.addEdge(EdgeHolder(point(0), point(1), point(2), point(3))); break;
                    default: return 1;
                }
            }
        }
        shape.normalize();
        edgeColoringSimple(shape, 3.0, seed);

        std::vector<float> samples(static_cast<std::size_t>(width) * height * 4);
        BitmapSection<float, 4> bitmap(samples.data(), width, height, Y_DOWNWARD);
        const Projection projection(Vector2(1, 1), Vector2(0, 0));
        const SDFTransformation transform(projection, Range(2 * spread));

        // Correct the fill signs before applying the post-process error correction.
        MSDFGeneratorConfig config;
        config.errorCorrection.mode = ErrorCorrectionConfig::DISABLED;
        generateMTSDF(bitmap, shape, transform, config);
        distanceSignCorrection(bitmap, shape, projection, .5f, FILL_NONZERO);
        msdfErrorCorrection(bitmap, shape, transform);
        for (std::size_t i = 0; i < samples.size(); ++i)
            pixels[i] = static_cast<std::uint8_t>(255 * std::max(0.f, std::min(1.f, samples[i])));
        return 0;
    } catch (...) {
        return 1;
    }
}
