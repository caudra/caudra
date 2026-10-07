// A schollz/progressbar bar, the dominant Go progress library.
//
// This one does redraw into a pipe, and it is the hardest observed case: it
// blanks each frame with spaces before drawing the next and emits no newline at
// all, so the whole run arrives as a single row many kilobytes wide.
package main

import (
	"time"

	"github.com/schollz/progressbar/v3"
)

func main() {
	bar := progressbar.NewOptions(200, progressbar.OptionSetDescription("Loading weights"))
	for i := 0; i < 200; i++ {
		_ = bar.Add(1)
		time.Sleep(3 * time.Millisecond)
	}
	_ = bar.Finish()
}
