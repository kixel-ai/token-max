// Every path and query on max.kixel.ai moves, permanently, to the same one on max.kixel.me.
export default {
	fetch(request) {
		const url = new URL(request.url);
		return Response.redirect(`https://max.kixel.me${url.pathname}${url.search}`, 301);
	},
};
