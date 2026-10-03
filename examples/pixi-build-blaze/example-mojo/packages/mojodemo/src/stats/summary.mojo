from geometry import Vec2


def mean(xs: List[Float64]) -> Float64:
    var total = 0.0
    for x in xs:
        total += x
    return total / Float64(len(xs))


def centroid(points: List[Vec2]) -> Vec2:
    var acc = Vec2(0, 0)
    for p in points:
        acc = acc + p
    var n = Float64(len(points))
    return Vec2(acc.x / n, acc.y / n)
