// hls.js types its main entry only; the light build exports the same API.
declare module "hls.js/light" {
	export * from "hls.js";
	export { default } from "hls.js";
}
