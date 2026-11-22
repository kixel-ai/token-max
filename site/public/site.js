// Enhancements only: without this script the tabs still switch and the terminal shows its whole run.

// Pick the install command for this OS.
const platform = navigator.userAgentData?.platform ?? navigator.platform ?? "";
if (/^win/i.test(platform)) document.getElementById("os-windows").checked = true;

// Copy the visible command.
const copy = document.querySelector(".copy");
copy.hidden = false;
copy.addEventListener("click", async () => {
	const windows = document.getElementById("os-windows").checked;
	const command = document.querySelector(windows ? ".windows .sel" : ".unix .sel");
	try {
		await navigator.clipboard.writeText(command.textContent);
		copy.textContent = "Copied";
	} catch {
		// Clipboard access denied: select the command so the reader can copy it.
		getSelection().selectAllChildren(command);
		copy.textContent = "Selected";
	}
	setTimeout(() => (copy.textContent = "Copy"), 1500);
});

// Play the recorded run once it scrolls into view, unless the reader prefers less motion.
// While it plays, the button skips to the whole run (WCAG 2.2.2); after, it replays.
const term = document.querySelector(".term");
const button = document.querySelector(".replay");
const steps = [...term.querySelectorAll(".step")];
// A finished run rests on the report's count of changes rather than its last line.
const screen = term.querySelector(".body");
const rest = () => {
	const top = term.querySelector(".rest").getBoundingClientRect().top;
	const start = screen.getBoundingClientRect().top + parseFloat(getComputedStyle(screen).paddingTop);
	screen.scrollBy(0, top - start);
};
if (matchMedia("(prefers-reduced-motion: reduce)").matches) {
	rest();
} else {
	let timers = [];
	const cancel = () => {
		timers.forEach(clearTimeout);
		timers = [];
		term.classList.remove("wait", "play");
	};
	const stop = () => {
		cancel();
		button.textContent = "Replay";
		rest();
	};
	// Each step shows at its --t and its typed answer --gap later, both set in style.css.
	const play = () => {
		cancel();
		for (const step of steps) step.classList.remove("shown", "answered");
		void term.offsetWidth; // restart the command's typing animation
		term.classList.add("play");
		let end = 0;
		for (const step of steps) {
			const style = getComputedStyle(step);
			const shown = parseFloat(style.getPropertyValue("--t")) * 1000;
			const answered = shown + parseFloat(style.getPropertyValue("--gap")) * 1000;
			timers.push(setTimeout(() => step.classList.add("shown"), shown));
			timers.push(setTimeout(() => step.classList.add("answered"), answered));
			end = Math.max(end, answered);
		}
		timers.push(setTimeout(stop, end + 500));
		button.textContent = "Skip";
		button.hidden = false;
	};
	term.classList.add("wait");
	new IntersectionObserver((entries, observer) => {
		if (!entries[0].isIntersecting) return;
		observer.disconnect();
		play();
	}, { threshold: 0.35 }).observe(term);
	button.addEventListener("click", () => (button.textContent === "Skip" ? stop() : play()));
}
