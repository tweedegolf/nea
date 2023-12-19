export type Request = {
	body: string;
	headers: Map<string, string>;
	method: string;
	path: string;
	version: string;
};

export type Response = {
	contentType: string;
	body: string;
	statusCode: string;
};

export type Service = {
	handleRequest: RequestHandler;
};

export type RequestHandler = (request: Request) => Promise<Response>;
