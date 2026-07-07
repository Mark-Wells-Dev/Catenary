package main

func main() {
	// Intentional diagnostic: gopls (via go/types) reports the type mismatch —
	// a string literal assigned to an int.
	var answer int = "not a number"
	_ = answer
}
