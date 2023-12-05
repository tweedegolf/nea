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

export type RequestHandler = (request: Request) => Response;
