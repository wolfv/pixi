#define PY_SSIZE_T_CLEAN
#include <Python.h>

static PyObject *greet(PyObject *self, PyObject *args) {
    const char *name = "world";
    if (!PyArg_ParseTuple(args, "|s", &name)) {
        return NULL;
    }
    return PyUnicode_FromFormat("Hello, %s! (from C in Python)", name);
}

static PyMethodDef methods[] = {
    {"greet", greet, METH_VARARGS, "Return a greeting."},
    {NULL, NULL, 0, NULL},
};

static struct PyModuleDef module = {PyModuleDef_HEAD_INIT, "_greet", NULL, -1, methods};

PyMODINIT_FUNC PyInit__greet(void) { return PyModule_Create(&module); }
