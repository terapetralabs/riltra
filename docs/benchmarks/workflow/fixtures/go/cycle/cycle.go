package main

func ping(n int) int {
	if n == 0 {
		return 0
	}
	return pong(n - 1)
}

func pong(n int) int {
	return ping(n)
}
