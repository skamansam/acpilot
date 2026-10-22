def 日本語の関数(value: int) -> int:
    return value + 1


def caller() -> int:
    banner = "😀 日本語 é"; return 日本語の関数(41)


def type_error() -> int:
    value: int = "not a number"
    return value
