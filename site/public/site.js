// Enhancements only: without this script the tabs still switch and the terminal shows its whole run.

// Pick the install command for this OS.
const platform = navigator.userAgentData?.platform ?? navigator.platform ?? "";
if (/^win/i.test(platform)) document.getElementById("os-windows").checked = true;

// Copy the visible command.
const copy = document.querySelector(".copy");
copy.hidden = false;
copy.addEventListener("click", async () => {
	const windows = document.getElementById("os-windows").checked;
	const command = document.querySelector(windows ? ".windows .sel" : ".unix .sel").textContent;
	await navigator.clipboard.writeText(command);
	copy.textContent = "Copied";
	setTimeout(() => (copy.textContent = "Copy"), 1500);
});

// Play the recorded run once it scrolls into view, unless the reader prefers less motion.
const term = document.querySelector(".term");
const replay = document.querySelector(".replay");
if (!matchMedia("(prefers-reduced-motion: reduce)").matches) {
	const play = () => {
		term.classList.remove("play");
		void term.offsetWidth; // restart the CSS animations
		term.classList.add("play");
	};
	term.classList.add("wait");
	new IntersectionObserver((entries, observer) => {
		if (!entries[0].isIntersecting) return;
		observer.disconnect();
		term.classList.remove("wait");
		play();
	}, { threshold: 0.35 }).observe(term);
	replay.hidden = false;
	replay.addEventListener("click", play);
}
