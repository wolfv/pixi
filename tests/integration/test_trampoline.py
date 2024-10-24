import json
from pathlib import Path

from .common import verify_cli_command, exec_extension, is_binary


def test_trampoline_respect_activation_variables(
    pixi: Path, tmp_path: Path, trampoline_channel: str
) -> None:
    env = {"PIXI_HOME": str(tmp_path)}

    dummy_b = tmp_path / "bin" / "dummy-b"

    verify_cli_command(
        [
            pixi,
            "global",
            "install",
            "--channel",
            trampoline_channel,
            "dummy-b",
        ],
        env=env,
    )

    assert is_binary(dummy_b)

    dummy_b_json = tmp_path / "bin" / "dummy-b.json"

    trampoline_metadata = json.loads(dummy_b_json.read_text())
    # get envs of the trampoline
    trampoline_env = trampoline_metadata["env"]
    assert trampoline_env["TRAMPOLINE_TEST_ENV"] == "teapot"
    assert "CONDA_PREFIX" in trampoline_env
    assert "PATH" in trampoline_env

    # verify that exe and root folder is correctly set to the original one
    original_dummy_b = tmp_path / "envs" / "dummy-b" / "bin" / "dummy-b"
    assert trampoline_metadata["exe"] == str(original_dummy_b)
    assert trampoline_metadata["path"] == str(original_dummy_b.parent)

    # now execute the binary
    verify_cli_command(
        [dummy_b], stdout_contains="Success: 'TRAMPOLINE_TEST_ENV' is set to the expected value."
    )


def test_trampoline_migrate_previous_script(
    pixi: Path, tmp_path: Path, trampoline_channel: str
) -> None:
    # this test will validate if new trampoline will migrate the previous way of running packages (using scripts)
    env = {"PIXI_HOME": str(tmp_path)}

    # create a dummy script that will act as already installed package
    dummy_b = tmp_path / "bin" / exec_extension("dummy-b")

    # now run install again, this time it should migrate the script to the new trampoline
    verify_cli_command(
        [
            pixi,
            "global",
            "install",
            "--channel",
            trampoline_channel,
            "dummy-b",
        ],
        env=env,
    )

    assert dummy_b.is_file()
    assert is_binary(dummy_b)

    dummy_b_json = tmp_path / "bin" / "dummy-b.json"

    assert dummy_b_json.is_file()
