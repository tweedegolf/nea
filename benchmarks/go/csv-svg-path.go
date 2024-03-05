package main

import (
	"fmt"
	"net/http"
	"io"
	"strings"
	"strconv"
)

import _ "embed"

//go:embed static/Cargo.toml
var cargoToml string

var workerCount = 2
var workerPool chan struct{}

func main() {
	// Create a buffered channel to limit the number of concurrent worker threads
	workerPool = make(chan struct{}, workerCount)

	// Handle requests using a single IO thread
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		// Acquire a worker thread from the pool
		workerPool <- struct{}{}

		// Handle the request in a goroutine
		{
			// Ensure the worker is released back to the pool when done
			defer func() {
				<-workerPool
			}()

			// Handle the actual request (in this case, just returning "Hello, World!")
			handleRequest(w, r)
		}
	})

	// Start the web server on port 8080
	fmt.Println("Server listening on :8080")
	http.ListenAndServe(":8080", nil)
}

func handleRequest(w http.ResponseWriter, r *http.Request) {
	body, err := io.ReadAll(r.Body)
	if err != nil {
		http.Error(w, "Error reading request body", http.StatusInternalServerError)
		return
	}

	pathString, err := generateSVGPath(string(body))
	if err != nil {
		http.Error(w, "Error processing request body", http.StatusBadRequest)
		return
	}

	response := fmt.Sprintf(`<svg width="100" height="100" xmlns="http://www.w3.org/2000/svg">
    <path d="%s" stroke="black" fill="transparent"/>
</svg>
`, pathString)

	_, _ = w.Write([]byte(response))
}

func generateSVGPath(requestBody string) (string, error) {
	lines := strings.Split(requestBody, "\n")

	var pathString strings.Builder
	pathString.WriteString("M 0 0 L")

	for _, line := range lines {
		parts := strings.SplitN(line, ", ", 2)
		if len(parts) != 2 {
			continue
		}

		x, errX := strconv.Atoi(parts[0])
		y, errY := strconv.Atoi(parts[1])

		if errX != nil || errY != nil {
			return "", fmt.Errorf("invalid input format")
		}

		_, _ = fmt.Fprintf(&pathString, " %d %d", x, y)
	}

	return pathString.String(), nil
}
