import numpy as np

from localflow.audio import rms_dbfs


def test_rms_dbfs_of_zero_audio_is_negative_infinity():
    assert rms_dbfs(np.zeros(4, dtype=np.float32)) == float("-inf")


def test_rms_dbfs_of_constant_audio():
    assert rms_dbfs(np.full(4, 0.5, dtype=np.float32)) == -6.020599913279624
