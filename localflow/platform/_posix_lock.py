def try_lock(fd: int) -> bool:
    import fcntl

    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        return False
    return True


def unlock(fd: int) -> None:
    import fcntl

    fcntl.flock(fd, fcntl.LOCK_UN)
