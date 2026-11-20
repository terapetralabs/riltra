def shared():
    return "alpha café"

def alpha():
    def outer():
        def inner():
            return "nested"
        return inner()
    return outer()
