// An mpb bar, the other widely used Go progress library.
//
// Measured rather than assumed: mpb checks whether the output is a terminal and
// stays silent on a pipe, unlike schollz/progressbar beside it.
package main

import (
	"time"

	"github.com/vbauerster/mpb/v8"
	"github.com/vbauerster/mpb/v8/decor"
)

func main() {
	progress := mpb.New(mpb.WithWidth(40))
	bar := progress.AddBar(200,
		mpb.PrependDecorators(decor.Name("Loading weights ")),
		mpb.AppendDecorators(decor.Percentage()))
	for i := 0; i < 200; i++ {
		bar.Increment()
		time.Sleep(3 * time.Millisecond)
	}
	progress.Wait()
}
