import {Request, Response} from '../types';

export const handleRequest = async (request: Request): Promise<Response> => {
	return {
		statusCode: '200 OK',
		contentType: 'text/plain',
		body: '[workspace]\n' +
			'members = [ \n' +
			'    "log",\n' +
			'    "shared",\n' +
			'    "roc_app",\n' +
			'    "nea",\n' +
			']\n' +
			'exclude = [\n' +
			'    "benchmarks",\n' +
			']\n' +
			'resolver = "2"\n',
	};
};
