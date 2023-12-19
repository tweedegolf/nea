import * as net from 'net';
import {Request, Service} from "./types";

const PORT = 8000;
const handlerName = process.argv[2] || 'average';
const handlerModule: Service = require(`./handlers/${handlerName}`);

const server = net.createServer((socket) => {
	socket.on('data', async (data) => {
		const requestString = data.toString();

		// Basic parsing of the request
		const [headerLines, body] = requestString.split('\r\n\r\n', 2);
		const lines = headerLines.split('\r\n');
		const [method, path, version] = lines[0].split(' ');
		const headers = new Map<string, string>();
		for (const line of lines.slice(1)) {
			const [key, value] = line.split(': ', 2);
			headers.set(key, value);
		}

		const request: Request = { body, headers, method, path, version };

		// Call the handler and get the response
		const { statusCode, contentType, body: responseBody } = await handlerModule.handleRequest(request);

		// Format the response
		const response = `HTTP/1.1 ${statusCode}\r\nContent-Type: ${contentType}\r\nContent-Length: ${responseBody.length}\r\n\r\n${responseBody}`;

		// Send the response back to the client
		socket.write(response, () => {
			socket.end();
		});
	});
});

server.listen(PORT);
