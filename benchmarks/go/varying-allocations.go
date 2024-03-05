package main

import (
	"fmt"
	"net/http"
	"strings"
	"strconv"
)

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
    // Get the path from the URL
	path := r.URL.Path

	// Split the path using "/"
	parts := strings.Split(path, "/")

	// The last part of the path should be the number
	if len(parts) > 0 {
		lastPart := parts[len(parts)-1]

		// Convert the last part to an integer
		capacity, err := strconv.Atoi(lastPart)
		if err != nil {
			http.Error(w, "Invalid ID", http.StatusBadRequest)
			return
		}

        // Allocate an array of N bytes using make
        byteArray := make([]byte, capacity)

        for i := range byteArray {
            byteArray[i] = 0xAA
        }

        _ = byteArray;
	} else {
		http.Error(w, "Invalid URL", http.StatusBadRequest)
	}
}
