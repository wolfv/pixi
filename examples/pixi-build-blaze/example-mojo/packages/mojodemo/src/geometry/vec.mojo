from std.math import sqrt


@fieldwise_init
struct Vec2(Copyable, Writable):
    var x: Float64
    var y: Float64

    def __add__(self, other: Self) -> Self:
        return Self(self.x + other.x, self.y + other.y)

    def dot(self, other: Self) -> Float64:
        return self.x * other.x + self.y * other.y

    def norm(self) -> Float64:
        return sqrt(self.dot(self))
