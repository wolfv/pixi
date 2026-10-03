from setuptools import Extension, setup

setup(
    packages=["pygreet"],
    package_dir={"": "src"},
    ext_modules=[Extension("pygreet._greet", ["src/pygreet/_greet.c"])],
)
